use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;

/// Number of consecutive identical tool calls to trigger loop detection
const TOOL_CALL_THRESHOLD: i32 = 5;
/// Number of characters per content chunk
const CHUNK_SIZE: usize = 50;
/// Step size for sliding window content detection
const STEP_SIZE: usize = 10;
/// Number of chunk repetitions to trigger loop detection
const CONTENT_REPETITION_THRESHOLD: usize = 10;
/// Maximum average distance (in bytes) between repeated chunks
const MAX_AVG_DISTANCE: f64 = 250.0;
/// Maximum history size before pruning (in bytes)
const MAX_HISTORY_SIZE: usize = 25000;
/// Amount to prune from history when it exceeds max size (in bytes)
const HISTORY_PRUNE_AMOUNT: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopType {
    ConsecutiveIdenticalToolCalls,
    RepetitiveContent,
}

pub struct LoopDetector {
    last_tool_key: Option<u64>,
    tool_repetition_count: i32,
    content_buffer: String,
    content_history: String,
    chunk_positions: HashMap<String, Vec<usize>>,
}

impl Default for LoopDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopDetector {
    pub fn new() -> Self {
        Self {
            last_tool_key: None,
            tool_repetition_count: 0,
            content_buffer: String::new(),
            content_history: String::new(),
            chunk_positions: HashMap::new(),
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn check_tool_call(&mut self, name: &str, args: &str) -> Option<LoopType> {
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        ":".hash(&mut hasher);
        args.hash(&mut hasher);
        let key = hasher.finish();

        if self.last_tool_key == Some(key) {
            self.tool_repetition_count += 1;
        } else {
            self.last_tool_key = Some(key);
            self.tool_repetition_count = 1;
        }

        // Reset content tracking on tool call (content loops are per-stream)
        self.content_buffer.clear();
        self.content_history.clear();
        self.chunk_positions.clear();

        if self.tool_repetition_count >= TOOL_CALL_THRESHOLD {
            Some(LoopType::ConsecutiveIdenticalToolCalls)
        } else {
            None
        }
    }

    pub fn check_content(&mut self, delta: &str) -> Option<LoopType> {
        self.content_buffer.push_str(delta);
        self.content_history.push_str(delta);
        let mut result = None;

        while self.content_buffer.chars().count() >= CHUNK_SIZE {
            let chunk: String = self.content_buffer.chars().take(CHUNK_SIZE).collect();

            let pos = self.content_history.len() - self.content_buffer.len();

            // Remove the chunk from buffer (handle multi-byte chars correctly)
            let byte_offset: usize = self
                .content_buffer
                .char_indices()
                .nth(STEP_SIZE)
                .map(|(i, _)| i)
                .unwrap_or(self.content_buffer.len());
            self.content_buffer = self.content_buffer[byte_offset..].to_string();

            self.chunk_positions
                .entry(chunk.clone())
                .or_default()
                .push(pos);
            let positions = &self.chunk_positions[&chunk];

            if positions.len() >= CONTENT_REPETITION_THRESHOLD {
                let recent_positions = &positions[positions.len() - CONTENT_REPETITION_THRESHOLD..];
                let mut total_dist = 0;
                for i in 1..CONTENT_REPETITION_THRESHOLD {
                    total_dist += recent_positions[i] - recent_positions[i - 1];
                }
                let avg_dist = total_dist as f64 / (CONTENT_REPETITION_THRESHOLD - 1) as f64;
                if avg_dist <= MAX_AVG_DISTANCE {
                    result = Some(LoopType::RepetitiveContent);
                    break;
                }
            }

            if self.content_history.len() > MAX_HISTORY_SIZE {
                // Find a valid char boundary for pruning
                let mut shift = HISTORY_PRUNE_AMOUNT;
                while !self.content_history.is_char_boundary(shift)
                    && shift < self.content_history.len()
                {
                    shift += 1;
                }
                if shift < self.content_history.len() {
                    self.content_history = self.content_history[shift..].to_string();

                    let mut new_map = HashMap::new();
                    for (h, pos_list) in self.chunk_positions.drain() {
                        let new_list: Vec<usize> = pos_list
                            .into_iter()
                            .filter(|&p| p >= shift)
                            .map(|p| p - shift)
                            .collect();
                        if !new_list.is_empty() {
                            new_map.insert(h, new_list);
                        }
                    }
                    self.chunk_positions = new_map;
                }
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_loop_detection() {
        let mut detector = LoopDetector::new();
        for _ in 0..4 {
            assert_eq!(detector.check_tool_call("ls", "."), None);
        }
        assert_eq!(
            detector.check_tool_call("ls", "."),
            Some(LoopType::ConsecutiveIdenticalToolCalls)
        );
    }

    #[test]
    fn test_tool_different_args_resets() {
        let mut detector = LoopDetector::new();
        for _ in 0..4 {
            detector.check_tool_call("ls", ".");
        }
        assert_eq!(detector.check_tool_call("ls", ".."), None);
        assert!(detector.last_tool_key.is_some());
        assert_eq!(detector.tool_repetition_count, 1);
    }

    #[test]
    fn test_content_loop_detection() {
        let mut detector = LoopDetector::new();
        let chunk = "This is a 50-character string that we will repeat!".to_string();
        assert_eq!(chunk.chars().count(), 50);

        for _ in 0..9 {
            let res = detector.check_content(&chunk);
            assert_eq!(res, None);
        }
        assert_eq!(
            detector.check_content(&chunk),
            Some(LoopType::RepetitiveContent)
        );
    }

    #[test]
    fn test_content_no_loop_varied() {
        let mut detector = LoopDetector::new();
        for i in 0..20 {
            let chunk = format!("This is chunk number {:03}, which makes it unique!!!", i);
            assert_eq!(chunk.chars().count(), 50);
            assert_eq!(detector.check_content(&chunk), None);
        }
    }

    #[test]
    fn test_content_buffer_accumulation() {
        let mut detector = LoopDetector::new();
        assert_eq!(detector.check_content("Short"), None);
        assert_eq!(detector.content_buffer, "Short");
        assert_eq!(
            detector.check_content("... and more content to reach fifty characters now."),
            None
        );
        assert_eq!(detector.content_history.len() >= 50, true);
    }
}
