use std::collections::VecDeque;

use sn2_types::MAX_POW_QUEUE_SIZE;

#[derive(Debug)]
pub(crate) struct PowItem {
    pub miner_uid: u16,
    pub validator_uid: u16,
    pub verified: bool,
    pub response_time: f64,
    pub proof_size: u64,
    pub previous_score: f64,
    pub maximum_score: f64,
    pub maximum_response_time: f64,
    pub minimum_response_time: f64,
    pub block_number: u64,
}

#[derive(Default)]
pub(crate) struct PowManager {
    queue: VecDeque<PowItem>,
}

impl PowManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, item: PowItem) {
        self.queue.push_back(item);
    }

    pub fn should_batch(&self) -> bool {
        self.queue.len() >= MAX_POW_QUEUE_SIZE
    }

    pub fn drain_batch(&mut self) -> Vec<PowItem> {
        self.queue
            .drain(..MAX_POW_QUEUE_SIZE.min(self.queue.len()))
            .collect()
    }

    pub fn prepare_inputs(items: &[PowItem]) -> serde_json::Value {
        let n = items.len();
        let mut maximum_score = Vec::with_capacity(n);
        let mut previous_score = Vec::with_capacity(n);
        let mut verified = Vec::with_capacity(n);
        let mut proof_size = Vec::with_capacity(n);
        let mut response_time = Vec::with_capacity(n);
        let mut maximum_response_time = Vec::with_capacity(n);
        let mut minimum_response_time = Vec::with_capacity(n);
        let mut block_number = Vec::with_capacity(n);
        let mut validator_uid = Vec::with_capacity(n);
        let mut miner_uid = Vec::with_capacity(n);

        for item in items {
            maximum_score.push(item.maximum_score);
            previous_score.push(item.previous_score);
            verified.push(if item.verified { 1u8 } else { 0u8 });
            proof_size.push(item.proof_size);
            response_time.push(item.response_time);
            maximum_response_time.push(item.maximum_response_time);
            minimum_response_time.push(item.minimum_response_time);
            block_number.push(item.block_number);
            validator_uid.push(item.validator_uid);
            miner_uid.push(item.miner_uid);
        }

        serde_json::json!({
            "maximum_score": maximum_score,
            "previous_score": previous_score,
            "verified": verified,
            "proof_size": proof_size,
            "response_time": response_time,
            "maximum_response_time": maximum_response_time,
            "minimum_response_time": minimum_response_time,
            "block_number": block_number,
            "validator_uid": validator_uid,
            "miner_uid": miner_uid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_item(uid: u16) -> PowItem {
        PowItem {
            miner_uid: uid,
            validator_uid: 0,
            verified: true,
            response_time: 1.0,
            proof_size: 100,
            previous_score: 0.0,
            maximum_score: 1.0,
            maximum_response_time: 2.0,
            minimum_response_time: 0.5,
            block_number: 1000,
        }
    }

    #[test]
    fn new_manager_is_empty() {
        let mgr = PowManager::new();
        assert!(!mgr.should_batch());
        assert_eq!(mgr.queue.len(), 0);
    }

    #[test]
    fn should_batch_at_threshold() {
        let mut mgr = PowManager::new();
        for i in 0..MAX_POW_QUEUE_SIZE {
            mgr.push(make_item(i as u16));
        }
        assert!(mgr.should_batch());
    }

    #[test]
    fn drain_batch_takes_up_to_max() {
        let mut mgr = PowManager::new();
        let count = MAX_POW_QUEUE_SIZE + 5;
        for i in 0..count {
            mgr.push(make_item(i as u16));
        }
        let batch = mgr.drain_batch();
        assert_eq!(batch.len(), MAX_POW_QUEUE_SIZE);
        assert_eq!(mgr.queue.len(), 5);
    }

    #[test]
    fn drain_batch_partial_when_under_threshold() {
        let mut mgr = PowManager::new();
        let n = MAX_POW_QUEUE_SIZE / 2;
        for i in 0..n {
            mgr.push(make_item(i as u16));
        }
        assert!(!mgr.should_batch());
        let batch = mgr.drain_batch();
        assert_eq!(batch.len(), n);
        assert_eq!(mgr.queue.len(), 0);
    }

    #[test]
    fn prepare_inputs_structure() {
        let items = vec![make_item(1), make_item(2)];
        let json = PowManager::prepare_inputs(&items);
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 10);
        assert_eq!(obj["miner_uid"].as_array().unwrap().len(), 2);
        assert_eq!(obj["verified"].as_array().unwrap()[0], 1);
    }
}
