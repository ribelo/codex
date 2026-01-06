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
    /// Track the hash of the last tool output
    last_output_hash: Option<u64>,
    /// Whether the last output differed from the one before it
    last_output_was_novel: bool,
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
            last_output_hash: None,
            last_output_was_novel: false, // First output doesn't count as novel
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Call this after a tool execution completes to track whether output varies.
    /// This helps distinguish legitimate polling (varied output) from stuck loops (same output).
    pub fn observe_output(&mut self, output: &str) {
        let mut hasher = DefaultHasher::new();
        output.hash(&mut hasher);
        let new_hash = hasher.finish();

        // Output is novel if it differs from the previous output
        // If this is the first output, it doesn't count as novel (no progress shown yet)
        if let Some(prev_hash) = self.last_output_hash {
            self.last_output_was_novel = prev_hash != new_hash;
        } else {
            // First output - don't mark as novel since we have no previous output to compare
            self.last_output_was_novel = false;
        }
        self.last_output_hash = Some(new_hash);
    }

    pub fn check_tool_call(&mut self, name: &str, args: &str) -> Option<LoopType> {
        let mut hasher = DefaultHasher::new();
        name.hash(&mut hasher);
        ":".hash(&mut hasher);
        // Normalize JSON args to handle key order differences
        // (e.g., {"a":1,"b":2} vs {"b":2,"a":1} should be considered identical)
        let normalized_args = Self::normalize_json_args(args);
        normalized_args.hash(&mut hasher);
        let key = hasher.finish();

        if self.last_tool_key == Some(key) {
            // Same tool call as before - but check if output was novel
            if self.last_output_was_novel {
                // Previous execution produced new data, so this is valid progress (polling)
                // Consume the novelty credit and reset count - novel output breaks the chain
                self.last_output_was_novel = false;
                self.tool_repetition_count = 1;
            } else {
                // Previous execution produced identical output - this is a stuck loop
                self.tool_repetition_count += 1;
            }
        } else {
            self.last_tool_key = Some(key);
            self.tool_repetition_count = 1;
            // Reset output tracking when switching tools
            self.last_output_hash = None;
            self.last_output_was_novel = false;
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

    /// Normalize JSON arguments by parsing and re-serializing with sorted keys.
    /// Falls back to the original string if parsing fails.
    fn normalize_json_args(args: &str) -> String {
        match serde_json::from_str::<serde_json::Value>(args) {
            Ok(value) => {
                // serde_json serializes object keys in insertion order,
                // but we need sorted order for consistent hashing.
                // The simplest approach: convert to string with sorted keys.
                Self::json_to_sorted_string(&value)
            }
            Err(_) => args.to_string(),
        }
    }

    /// Recursively serialize JSON value with sorted object keys.
    fn json_to_sorted_string(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Object(map) => {
                let mut pairs: Vec<_> = map.iter().collect();
                pairs.sort_by_key(|(k, _)| *k);
                let inner: Vec<String> = pairs
                    .into_iter()
                    .map(|(k, v)| {
                        // Use serde_json::to_string for keys to handle escaping correctly
                        let key_str =
                            serde_json::to_string(k).unwrap_or_else(|_| format!("\"{k}\""));
                        format!("{key_str}:{}", Self::json_to_sorted_string(v))
                    })
                    .collect();
                format!("{{{}}}", inner.join(","))
            }
            serde_json::Value::Array(arr) => {
                let inner: Vec<String> = arr.iter().map(Self::json_to_sorted_string).collect();
                format!("[{}]", inner.join(","))
            }
            _ => value.to_string(),
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
        let output = "same output";

        // First 4 calls with identical output
        for i in 0..4 {
            assert_eq!(
                detector.check_tool_call("ls", "."),
                None,
                "call {} should not trigger",
                i + 1
            );
            detector.observe_output(output);
        }

        // 5th call should trigger loop detection
        assert_eq!(
            detector.check_tool_call("ls", "."),
            Some(LoopType::ConsecutiveIdenticalToolCalls)
        );
    }

    #[test]
    fn test_tool_loop_with_identical_output() {
        let mut detector = LoopDetector::new();
        let output = "same output";

        // Simulate polling with identical output - should trigger
        for i in 0..4 {
            assert_eq!(
                detector.check_tool_call("ls", "."),
                None,
                "call {} should not trigger",
                i + 1
            );
            detector.observe_output(output);
        }

        // 5th call triggers detection
        assert_eq!(
            detector.check_tool_call("ls", "."),
            Some(LoopType::ConsecutiveIdenticalToolCalls)
        );
    }

    #[test]
    fn test_tool_polling_with_varied_output() {
        let mut detector = LoopDetector::new();

        // Simulate polling with varied output - should never trigger
        let args = r#"{"session_id": 1}"#; // Constant arguments
        for i in 0..20 {
            // First observe novel output from the previous call
            detector.observe_output(&format!("output line {i}"));
            // Then make the same tool call
            assert_eq!(detector.check_tool_call("write_stdin", args), None);
        }
    }

    #[test]
    fn test_tool_output_varies_after_identical_calls() {
        let mut detector = LoopDetector::new();

        // First 3 calls with identical output
        for i in 0..3 {
            assert_eq!(
                detector.check_tool_call("ls", "."),
                None,
                "identical call {}",
                i + 1
            );
            detector.observe_output("same");
        }

        // 4th call: output changes to "different"
        assert_eq!(detector.check_tool_call("ls", "."), None);
        detector.observe_output("different");

        // Now we can have many more calls with "different" output
        // and they won't trigger because output keeps being novel
        for i in 0..10 {
            assert_eq!(
                detector.check_tool_call("ls", "."),
                None,
                "different call {}",
                i + 1
            );
            detector.observe_output(&format!("different-{i}"));
        }
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
            let chunk = format!("This is chunk number {i:03}, which makes it unique!!!");
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
        assert!(detector.content_history.len() >= 50);
    }
}
