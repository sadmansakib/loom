//! Persistence-compatibility tests for [`TaskRecord`]'s per-task effort
//! field: records written before the field existed must deserialize with
//! `effort: None`, and new records must round-trip it.

use loom_kernel::id::TaskId;
use loom_kernel::session::{TaskRecord, TaskStatus};

fn v0_body(id: &str) -> String {
    format!(
        "{{\"id\":\"{id}\",\"status\":\"running\",\"turns\":0,\
         \"usage\":{{\"input_tokens\":0,\"output_tokens\":0}},\
         \"started_at_millis\":1000,\"completed_at_millis\":null,\
         \"end_reason\":null}}"
    )
}

#[test]
fn pre_effort_task_record_deserializes_with_none() {
    let id = TaskId::new().to_string();
    let record: TaskRecord = serde_json::from_str(&v0_body(&id)).expect("v0 body parses");
    assert_eq!(record.effort, None);
    assert_eq!(record.status, TaskStatus::Running);
}

#[test]
fn effort_round_trips_through_json() {
    let mut record = TaskRecord::new(TaskId::new(), 1000);
    record.effort = Some("high".to_owned());
    let text = serde_json::to_string(&record).expect("serializes");
    let back: TaskRecord = serde_json::from_str(&text).expect("deserializes");
    assert_eq!(back.effort.as_deref(), Some("high"));
}
