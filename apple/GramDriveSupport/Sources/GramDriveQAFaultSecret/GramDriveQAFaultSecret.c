#include "GramDriveQAFaultSecret.h"

#ifndef GRAMDRIVE_QA_FAULT_SECRET
#error "QA fault-control target requires a per-build secret"
#endif

const char *gramdrive_qa_fault_secret(void) {
    return GRAMDRIVE_QA_FAULT_SECRET;
}
