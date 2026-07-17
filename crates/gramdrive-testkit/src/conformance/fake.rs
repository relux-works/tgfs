//! The deterministic fake's side of the conformance suite.
//!
//! [`FakeHarness`] is the first [`SourceHarness`], and for now the only one —
//! `gramdrive-source-tdjson` and `gramdrive-source-remote` will write their
//! own, and the suite will not change when they do. It is also the worked
//! example: a harness author reading this file is reading the whole of what
//! their backend owes the suite.
//!
//! # It supports everything, which is the point of it
//!
//! A `tdjson` source against a live account will decline capabilities — you
//! cannot ask Telegram for a flood wait on cue. The fake declines nothing:
//! its whole reason to exist is that every scripted event is reachable, so
//! every case runs and every clause is actually exercised somewhere. A skip
//! here would mean a clause no implementation has ever been held to.
//!
//! # Why the mutation plan is compiled, not applied
//!
//! [`SourceScript`] is immutable once built, and the fake advances through
//! pre-validated revisions rather than mutating a tree at test time — that is
//! what makes it deterministic. So [`Setup::plan`] is compiled here into one
//! change batch per mutation, in order, and [`Control::advance`] is
//! `FakeSource::advance`: batch `k` moves the source from revision `k` to
//! `k + 1`. The plan's order *is* the revision order, which is exactly why the
//! harness contract asks for the plan up front rather than taking mutations
//! one at a time.

use std::future::Future;
use std::sync::Arc;

use gramdrive_model::identity::{AccountScope, ItemId};
use gramdrive_model::version::ContentVersion;
use gramdrive_source::{DirectoryKind, FileKind, ItemChange, SourceError, SourceItem};

use crate::conformance::harness::{
    Capability, Control, Landmarks, Mutation, Perturbation, Setup, SourceHarness, Staged, WorldSpec,
};
use crate::conformance::report::HarnessError;
use crate::exec;
use crate::fake::FakeSource;
use crate::fault::{Fault, Occurrence, Operation};
use crate::fixture;
use crate::script::{ScriptBuilder, SourceScript};

/// The chat whose year directories are the listing the paging cases page.
const LISTING_CHAT: i64 = 100;
/// The chat with nothing in it.
const EMPTY_CHAT: i64 = 101;
/// The chat holding the world's files.
const FILES_CHAT: i64 = 102;
/// The year the world's files live under.
const FILES_YEAR: u16 = 2026;
/// The message the world's attachments hang off.
const FILES_MESSAGE: i64 = 5;
/// The first year the listing's children cover.
const FIRST_YEAR: u16 = 2000;
/// The year of the child that only appears once the world moves.
const APPEARING_YEAR: u16 = 2400;
/// A chat that does not exist, for the absent-item cases.
const ABSENT_CHAT: i64 = 999;

/// Suspension points a "slow" call yields before answering.
///
/// More than one: a case that abandons after a single poll must still find
/// the call in flight.
const SLOW_YIELDS: u32 = 3;

/// The deterministic fake's [`SourceHarness`].
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeHarness {
    scope: Option<AccountScope>,
}

impl FakeHarness {
    /// A harness staging worlds under the testkit's canonical scope.
    pub fn new() -> Self {
        Self { scope: None }
    }

    /// A harness staging worlds under `scope`.
    ///
    /// For a caller that wants two fakes whose cursors are foreign to each
    /// other; the suite itself never needs it.
    pub fn with_scope(scope: AccountScope) -> Self {
        Self { scope: Some(scope) }
    }

    fn scope(&self) -> AccountScope {
        self.scope.unwrap_or_else(fixture::scope)
    }
}

impl SourceHarness for FakeHarness {
    type Source = FakeSource;

    fn name(&self) -> &str {
        "gramdrive-testkit fake"
    }

    fn supports(&self, _capability: Capability) -> bool {
        // Everything. A scripted backend that could not stage a case would be
        // a scripted backend with a gap in its script.
        true
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        exec::drive(future)
    }

    fn stage(
        &self,
        world: &WorldSpec,
        setup: &Setup,
    ) -> Result<Staged<Self::Source>, HarnessError> {
        let scope = self.scope();
        let landmarks = landmarks(scope, world)?;
        let script = script(scope, world, setup, &landmarks)?;
        let source = Arc::new(FakeSource::new(script));

        Ok(Staged {
            control: Box::new(Plan {
                source: Arc::clone(&source),
            }),
            source,
            landmarks,
        })
    }
}

/// Walks the compiled plan: one batch per planned mutation, in order.
#[derive(Debug)]
struct Plan {
    source: Arc<FakeSource>,
}

impl Control for Plan {
    fn advance(&self) -> Result<bool, HarnessError> {
        Ok(self.source.advance())
    }
}

/// Where the fake puts each part of the world.
fn landmarks(scope: AccountScope, world: &WorldSpec) -> Result<Landmarks, HarnessError> {
    let children = (0..world.listing_children)
        .map(|index| listing_child(scope, index))
        .collect::<Result<Vec<_>, _>>()?;
    let Some(removable) = children.last().cloned() else {
        return Err(HarnessError::new(
            "the world asks for a listing with no children; the paging cases need some",
        ));
    };

    Ok(Landmarks {
        root: fixture::account_root_id(scope),
        listing: fixture::chat_id(scope, LISTING_CHAT),
        listing_children: children,
        removable_child: removable,
        appearing_child: fixture::year_dir_id(scope, LISTING_CHAT, APPEARING_YEAR),
        empty_directory: fixture::chat_id(scope, EMPTY_CHAT),
        file_parent: fixture::media_dir_id(scope, FILES_CHAT, FILES_YEAR),
        file: fixture::attachment_id(scope, FILES_CHAT, FILES_MESSAGE, 0),
        file_version: version(world.file_version)?,
        next_file_version: version(world.next_file_version)?,
        restricted_file: Some(fixture::attachment_id(scope, FILES_CHAT, FILES_MESSAGE, 1)),
        absent: fixture::chat_id(scope, ABSENT_CHAT),
    })
}

/// The `index`-th child of the listing directory.
fn listing_child(scope: AccountScope, index: u32) -> Result<ItemId, HarnessError> {
    let offset = u16::try_from(index).map_err(|_| {
        HarnessError::new(format!(
            "the world asks for {index} listing children; the fake numbers them by year"
        ))
    })?;
    Ok(fixture::year_dir_id(
        scope,
        LISTING_CHAT,
        FIRST_YEAR + offset,
    ))
}

/// The whole world, written down.
fn script(
    scope: AccountScope,
    world: &WorldSpec,
    setup: &Setup,
    landmarks: &Landmarks,
) -> Result<SourceScript, HarnessError> {
    let root = fixture::account_root_id(scope);
    let mut builder = ScriptBuilder::new(scope)
        .item(directory(
            root.clone(),
            None,
            "Account",
            "m-root",
            DirectoryKind::Root,
        )?)
        // The listing the paging cases walk.
        .item(directory(
            landmarks.listing.clone(),
            Some(root.clone()),
            "Listing",
            "m-listing",
            DirectoryKind::Chat,
        )?)
        // A directory with nothing in it.
        .item(directory(
            landmarks.empty_directory.clone(),
            Some(root.clone()),
            "Empty",
            "m-empty",
            DirectoryKind::Chat,
        )?)
        // The chat holding the world's files, and the path down to them.
        .item(directory(
            fixture::chat_id(scope, FILES_CHAT),
            Some(root),
            "Files",
            "m-files",
            DirectoryKind::Chat,
        )?)
        .item(directory(
            fixture::year_dir_id(scope, FILES_CHAT, FILES_YEAR),
            Some(fixture::chat_id(scope, FILES_CHAT)),
            "2026",
            "m-files-year",
            DirectoryKind::Year,
        )?)
        .item(directory(
            landmarks.file_parent.clone(),
            Some(fixture::year_dir_id(scope, FILES_CHAT, FILES_YEAR)),
            "media",
            "m-files-media",
            DirectoryKind::Media,
        )?);

    for (index, child) in landmarks.listing_children.iter().enumerate() {
        builder = builder.item(directory(
            child.clone(),
            Some(landmarks.listing.clone()),
            &format!("{}", FIRST_YEAR as usize + index),
            &format!("m-child-{index}"),
            DirectoryKind::Year,
        )?);
    }

    builder = builder
        .item(file_at(
            landmarks,
            world,
            world.file_version,
            world.file_bytes.len() as u64,
        )?)
        .content(
            &landmarks.file,
            version(world.file_version)?,
            world.file_bytes.to_vec(),
        )
        // Registered up front: the fake must be able to answer a fetch pinned
        // to either version, including after the content has moved on.
        .content(
            &landmarks.file,
            version(world.next_file_version)?,
            world.next_file_bytes.to_vec(),
        );

    if let Some(restricted) = &landmarks.restricted_file {
        builder = builder.item(
            fixture::restricted_file(
                restricted.clone(),
                landmarks.file_parent.clone(),
                "restricted.bin",
                "m-restricted",
                "restricted-c1",
                16,
                FileKind::Attachment,
            )
            .map_err(|error| HarnessError::new(format!("restricted fixture: {error}")))?,
        );
    }

    for perturbation in &setup.arm {
        builder = builder.fault(fault(perturbation, landmarks, world)?);
    }
    for mutation in &setup.plan {
        builder = builder.batch(batch(*mutation, landmarks, world)?);
    }

    builder
        .build()
        .map_err(|error| HarnessError::new(format!("the staged world is not playable: {error}")))
}

/// One planned mutation, as the change batch that performs it.
fn batch(
    mutation: Mutation,
    landmarks: &Landmarks,
    world: &WorldSpec,
) -> Result<Vec<ItemChange>, HarnessError> {
    Ok(match mutation {
        Mutation::ChildAppears => vec![ItemChange::Upserted(directory(
            landmarks.appearing_child.clone(),
            Some(landmarks.listing.clone()),
            "2400",
            "m-appearing",
            DirectoryKind::Year,
        )?)],
        Mutation::ChildRemoved => vec![ItemChange::Removed(landmarks.removable_child.clone())],
        Mutation::ContentChanges => vec![ItemChange::Upserted(file_at(
            landmarks,
            world,
            world.next_file_version,
            world.next_file_bytes.len() as u64,
        )?)],
    })
}

/// One armed perturbation, as the fault that stages it.
fn fault(
    perturbation: &Perturbation,
    landmarks: &Landmarks,
    world: &WorldSpec,
) -> Result<Fault, HarnessError> {
    Ok(match perturbation {
        Perturbation::Unreachable { operation } => Fault::on(*operation)
            .occurrence(Occurrence::Nth(1))
            .fail(SourceError::Unavailable {
                detail: "the conformance harness took the source offline for one call".to_owned(),
            }),
        Perturbation::ReferenceExpired { operation } => Fault::on(*operation)
            .occurrence(Occurrence::Nth(1))
            .fail(SourceError::StaleReference {
                detail: "the conformance harness expired the source's content reference".to_owned(),
            }),
        Perturbation::RateLimited {
            operation,
            retry_after,
        } => Fault::on(*operation)
            .occurrence(Occurrence::Nth(1))
            .fail(SourceError::RateLimited {
                retry_after: *retry_after,
                detail: "the conformance harness throttled the source".to_owned(),
            }),
        Perturbation::AuthRevoked { operation } => {
            Fault::on(*operation).fail(SourceError::AuthRequired {
                detail: "the conformance harness revoked the source's authorization".to_owned(),
            })
        }
        // A delay is a count of suspension points, not a duration — see
        // `crate::fault`. It is what makes the call droppable in flight.
        Perturbation::Slow { operation } => Fault::on(*operation).delay(SLOW_YIELDS),
        Perturbation::FetchRacesContentChange { after_bytes } => Fault::on(Operation::Fetch)
            .for_item(landmarks.file.clone())
            .version_race(*after_bytes, Some(version(world.next_file_version)?)),
    })
}

/// The world's file, as of `content_version`.
fn file_at(
    landmarks: &Landmarks,
    _world: &WorldSpec,
    content_version: &str,
    size: u64,
) -> Result<SourceItem, HarnessError> {
    fixture::file(
        landmarks.file.clone(),
        landmarks.file_parent.clone(),
        "payload.bin",
        // The metadata version moves with the content: an item whose bytes
        // changed is an item that changed.
        &format!("m-file-{content_version}"),
        content_version,
        size,
        FileKind::Attachment,
    )
    .map_err(|error| HarnessError::new(format!("file fixture: {error}")))
}

fn directory(
    id: ItemId,
    parent: Option<ItemId>,
    name: &str,
    metadata_version: &str,
    kind: DirectoryKind,
) -> Result<SourceItem, HarnessError> {
    fixture::directory(id, parent, name, metadata_version, kind)
        .map_err(|error| HarnessError::new(format!("directory fixture: {error}")))
}

fn version(token: &str) -> Result<ContentVersion, HarnessError> {
    ContentVersion::new(token)
        .map_err(|error| HarnessError::new(format!("content version {token:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::harness::WORLD;
    use gramdrive_source::DriveSource;

    #[test]
    fn a_plain_world_holds_every_landmark_the_suite_addresses() {
        let harness = FakeHarness::new();
        let staged = harness
            .stage(&WORLD, &Setup::new())
            .expect("the plain world stages");

        assert_eq!(staged.source.scope(), fixture::scope());
        assert_eq!(
            staged.landmarks.listing_children.len(),
            WORLD.listing_children as usize
        );
        assert!(
            staged
                .landmarks
                .listing_children
                .contains(&staged.landmarks.removable_child),
            "the removable child must be one of the listing's own"
        );
        assert!(
            !staged
                .landmarks
                .listing_children
                .contains(&staged.landmarks.appearing_child),
            "the appearing child must not be there until it appears"
        );
        assert!(staged.landmarks.restricted_file.is_some());
    }

    #[test]
    fn the_plan_becomes_one_revision_per_mutation_in_order() {
        let harness = FakeHarness::new();
        let staged = harness
            .stage(
                &WORLD,
                &Setup::new()
                    .plan(Mutation::ChildAppears)
                    .plan(Mutation::ContentChanges),
            )
            .expect("a two-step plan stages");

        assert_eq!(staged.source.revision(), 0);
        assert_eq!(staged.control.advance(), Ok(true));
        assert_eq!(staged.source.revision(), 1, "the first mutation landed");
        assert_eq!(staged.control.advance(), Ok(true));
        assert_eq!(staged.source.revision(), 2, "the second followed it");
        assert_eq!(
            staged.control.advance(),
            Ok(false),
            "a drained plan says so rather than inventing a step"
        );
    }

    #[test]
    fn a_planned_child_is_absent_until_the_plan_advances() {
        let harness = FakeHarness::new();
        let staged = harness
            .stage(&WORLD, &Setup::new().plan(Mutation::ChildAppears))
            .expect("the world stages");

        let before = exec::drive(staged.source.children(
            staged.landmarks.listing.clone(),
            gramdrive_source::PageRequest::first(std::num::NonZeroU32::new(64).expect("non-zero")),
        ))
        .expect("the listing enumerates");
        assert_eq!(before.items.len(), WORLD.listing_children as usize);

        staged.control.advance().expect("the plan advances");

        let after = exec::drive(staged.source.children(
            staged.landmarks.listing.clone(),
            gramdrive_source::PageRequest::first(std::num::NonZeroU32::new(64).expect("non-zero")),
        ))
        .expect("the listing enumerates");
        assert_eq!(after.items.len(), WORLD.listing_children as usize + 1);
        assert!(
            after
                .items
                .iter()
                .any(|item| item.id == staged.landmarks.appearing_child),
            "the planned child appeared"
        );
    }

    #[test]
    fn the_file_serves_both_of_its_versions_bytes() {
        // The stale-pin case needs the superseded version to still be
        // answerable — with a conflict, not with missing data.
        let harness = FakeHarness::new();
        let staged = harness
            .stage(&WORLD, &Setup::new().plan(Mutation::ContentChanges))
            .expect("the world stages");

        let script = staged.source.script();
        assert_eq!(
            script.blob(
                &staged.landmarks.file,
                &version(WORLD.file_version).expect("valid token")
            ),
            Some(WORLD.file_bytes)
        );
        assert_eq!(
            script.blob(
                &staged.landmarks.file,
                &version(WORLD.next_file_version).expect("valid token")
            ),
            Some(WORLD.next_file_bytes)
        );
    }

    #[test]
    fn an_armed_fault_reaches_the_source() {
        let harness = FakeHarness::new();
        let staged = harness
            .stage(
                &WORLD,
                &Setup::new().arm(Perturbation::AuthRevoked {
                    operation: Operation::Root,
                }),
            )
            .expect("the world stages");

        let error = exec::drive(staged.source.root()).expect_err("authorization was revoked");
        assert!(
            matches!(error, SourceError::AuthRequired { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_harness_declines_nothing() {
        let harness = FakeHarness::new();
        for capability in [
            Capability::Mutation,
            Capability::FaultInjection,
            Capability::Latency,
            Capability::VersionRace,
            Capability::RestrictedContent,
        ] {
            assert!(
                harness.supports(capability),
                "the fake exists so that no clause goes unexercised, but it declines \
                 {capability}"
            );
        }
    }
}
