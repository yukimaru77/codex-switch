use super::*;

#[test]
fn output_batch_is_bounded_and_reports_omission() {
    let mut batch = OutputBatch::default();
    batch.push(&vec![b'x'; MAX_EVENT_BYTES + 1024]);
    let summary = batch.take_summary();
    assert!(summary.len() <= MAX_EVENT_BYTES);
    assert!(summary.contains("1024 bytes omitted"));
    assert!(batch.is_empty());
}
