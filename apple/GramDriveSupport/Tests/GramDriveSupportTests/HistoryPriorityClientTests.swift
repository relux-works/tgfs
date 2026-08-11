import Testing

@testable import GramDriveSupport

@Suite("History-priority client queue")
struct HistoryPriorityClientTests {
    @Test("Visible saturation evicts requested work and stays bounded")
    func visibleSaturationEvictsRequested() {
        var queue = HistoryPriorityPendingQueue(limit: 3)
        queue.enqueue(request(chatId: 1, priority: .requested))
        queue.enqueue(request(chatId: 2, priority: .requested))
        queue.enqueue(request(chatId: 3, priority: .visible))

        queue.enqueue(request(chatId: 4, priority: .visible))

        #expect(queue.entries.count == 3)
        #expect(queue.entries.contains { $0.chatId == 4 && $0.priority == .visible })
        #expect(!queue.entries.contains { $0.chatId == 1 })
        #expect(queue.popNext()?.chatId == 4)
        #expect(queue.popNext()?.chatId == 3)
    }

    @Test("Newest visible state is admitted when every slot is visible")
    func newestVisibleDisplacesOldestVisible() {
        var queue = HistoryPriorityPendingQueue(limit: 2)
        queue.enqueue(request(chatId: 10, priority: .visible))
        queue.enqueue(request(chatId: 20, priority: .visible))

        queue.enqueue(request(chatId: 30, priority: .visible))

        #expect(queue.entries.map(\.chatId) == [30, 20])
        #expect(queue.entries.count == 2)
    }

    @Test("Same-chat state coalesces even when saturated")
    func sameChatCoalescesNewestState() {
        var queue = HistoryPriorityPendingQueue(limit: 2)
        queue.enqueue(request(chatId: 10, priority: .requested))
        queue.enqueue(request(chatId: 20, priority: .visible))

        queue.enqueue(request(chatId: 10, priority: .visible))
        queue.enqueue(request(chatId: 20, priority: .background))

        #expect(queue.entries.count == 2)
        #expect(queue.entries.first { $0.chatId == 10 }?.priority == .visible)
        #expect(queue.entries.first { $0.chatId == 20 }?.priority == .background)
        #expect(queue.popNext()?.chatId == 10)
    }

    private func request(chatId: Int64, priority: HistoryPriorityHint) -> HistoryPriorityRequest {
        HistoryPriorityRequest(accountId: 42, chatId: chatId, priority: priority)
    }
}
