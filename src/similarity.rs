use crate::normalization::{analyze_json_structure, normalize_content, normalize_tags};
use nostr_sdk::prelude::*;
use std::collections::{HashMap, HashSet};

/// Feature vector extracted from an event for similarity comparison
#[derive(Debug, Clone)]
pub struct EventFeatures {
    /// Normalized tag patterns (e.g., "e:<HEX_ID>", "d:value")
    pub tag_patterns: HashSet<String>,
    /// Normalized content string
    pub normalized_content: String,
}

impl EventFeatures {
    /// Extract features from a Nostr event
    pub fn from_event(event: &Event) -> Self {
        let tags_vec: Vec<_> = event.tags.iter().cloned().collect();
        let tag_patterns = normalize_tags(&tags_vec).into_iter().collect();
        let normalized_content = normalize_content(&event.content);

        Self {
            tag_patterns,
            normalized_content,
        }
    }

    /// Convert features to a dense vector for Qdrant storage.
    ///
    /// Vector dimension: 64
    /// - 0-31: Feature-hashed tag names (32 buckets)
    /// - 32-35: Content encoding detection
    /// - 36-63: Content structure features
    pub fn to_vector(&self) -> Vec<f32> {
        let mut vector = Vec::with_capacity(crate::qdrant_client::vector_size());

        // ===== TAG FEATURES (0-31): Feature hashing =====
        // Extract unique tag names and hash to 32 buckets
        let mut tag_buckets = [0.0f32; 32];

        let unique_tag_names: HashSet<String> = self
            .tag_patterns
            .iter()
            .filter_map(|pattern| {
                // Extract tag name (before first ':')
                pattern.split(':').next().map(String::from)
            })
            .collect();

        for tag_name in unique_tag_names {
            // Simple hash: sum of char codes modulo 32
            let hash = tag_name.chars().map(|c| c as u32).sum::<u32>() % 32;
            tag_buckets[hash as usize] = 1.0;
        }

        vector.extend_from_slice(&tag_buckets);

        // ===== CONTENT ENCODING DETECTION (32-35): 4 dimensions =====
        let content = &self.normalized_content;
        let content_len = content.len() as f32;

        if content_len > 0.0 {
            // Hex-only ratio (0-9a-f characters)
            let hex_chars = content.chars().filter(|c| c.is_ascii_hexdigit()).count() as f32;
            let hex_ratio = hex_chars / content_len;

            // Base64-like ratio (A-Za-z0-9+/= characters)
            let base64_chars = content
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
                .count() as f32;
            let base64_ratio = base64_chars / content_len;

            // Alphabetic ratio (works for all languages: Chinese, Spanish, etc.)
            let alpha_chars = content.chars().filter(|c| c.is_alphabetic()).count() as f32;
            let alpha_ratio = alpha_chars / content_len;

            // Character diversity (unique chars / total chars)
            let unique_chars = content.chars().collect::<HashSet<_>>().len() as f32;
            let char_diversity = unique_chars / content_len;

            vector.push(hex_ratio);
            vector.push(base64_ratio);
            vector.push(alpha_ratio);
            vector.push(char_diversity);
        } else {
            // Empty content
            vector.extend_from_slice(&[0.0, 0.0, 0.0, 0.0]);
        }

        // ===== CONTENT STRUCTURE (36-39): 4 dimensions =====
        vector.push((content_len / 1000.0).min(1.0)); // Normalized length (0-1000 chars)
        vector.push(if content_len == 0.0 { 1.0 } else { 0.0 }); // Empty content
        vector.push(if content_len > 500.0 { 1.0 } else { 0.0 }); // Long content
        vector.push(if content_len > 100.0 && content_len < 500.0 {
            1.0
        } else {
            0.0
        }); // Medium content

        // Content pattern features
        let has_hex_id = self.normalized_content.contains("<HEX_ID>");
        let has_nip19 = self.normalized_content.contains("<NIP19>");
        let has_timestamp = self.normalized_content.contains("<TIMESTAMP>");
        let has_url = self.normalized_content.contains("<URL>");

        vector.push(if has_hex_id { 1.0 } else { 0.0 });
        vector.push(if has_nip19 { 1.0 } else { 0.0 });
        vector.push(if has_timestamp { 1.0 } else { 0.0 });
        vector.push(if has_url { 1.0 } else { 0.0 });

        // Word count buckets (next 5 dimensions)
        let word_count = self.normalized_content.split_whitespace().count() as f32;
        vector.push(if word_count == 0.0 { 1.0 } else { 0.0 });
        vector.push(if word_count > 0.0 && word_count <= 10.0 {
            1.0
        } else {
            0.0
        });
        vector.push(if word_count > 10.0 && word_count <= 50.0 {
            1.0
        } else {
            0.0
        });
        vector.push(if word_count > 50.0 && word_count <= 200.0 {
            1.0
        } else {
            0.0
        });
        vector.push(if word_count > 200.0 { 1.0 } else { 0.0 });

        // Adaptive JSON structure features (10 dimensions)
        let json_struct = analyze_json_structure(&self.normalized_content);

        vector.push(if json_struct.is_valid_json { 1.0 } else { 0.0 });
        vector.push(if json_struct.is_object { 1.0 } else { 0.0 });
        vector.push(if json_struct.is_array { 1.0 } else { 0.0 });
        vector.push((json_struct.top_level_key_count as f32 / 20.0).min(1.0)); // Normalized 0-20 keys
        vector.push((json_struct.nested_key_count as f32 / 50.0).min(1.0)); // Normalized 0-50 nested keys
        vector.push((json_struct.max_nesting_depth as f32 / 5.0).min(1.0)); // Normalized 0-5 depth
        vector.push(if json_struct.has_arrays { 1.0 } else { 0.0 });
        vector.push(json_struct.string_value_ratio);
        vector.push(json_struct.number_value_ratio);
        vector.push(json_struct.bool_value_ratio);

        // Padding to reach exactly 64 dimensions
        while vector.len() < crate::qdrant_client::vector_size() {
            vector.push(0.0);
        }

        // Normalize to unit length (required for Cosine distance)
        let magnitude: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            vector.iter_mut().for_each(|x| *x /= magnitude);
        }

        vector
    }
}

/// Compute Jaccard similarity between two sets.
///
/// Returns a value between 0.0 (no overlap) and 1.0 (identical).
///
/// # Note
///
/// Currently unused - clustering now uses Qdrant's native similarity search.
/// Kept for potential future use and testing.
#[allow(dead_code)]
fn jaccard_similarity(set1: &HashSet<String>, set2: &HashSet<String>) -> f64 {
    if set1.is_empty() && set2.is_empty() {
        return 1.0; // Both empty = identical
    }

    let intersection = set1.intersection(set2).count();
    let union = set1.len() + set2.len() - intersection;

    if union == 0 {
        return 0.0;
    }

    intersection as f64 / union as f64
}

/// Compute similarity between two events.
///
/// Returns a value between 0.0 (completely different) and 1.0 (identical).
///
/// The similarity is a weighted combination of:
/// - Tag pattern similarity (Jaccard index): 60% weight
/// - Content structure similarity (Jaro-Winkler): 40% weight
///
/// # Note
///
/// Currently unused - clustering now uses Qdrant's cosine similarity on vector embeddings.
/// Kept for potential future use and testing.
#[allow(dead_code)]
pub fn compute_similarity(features1: &EventFeatures, features2: &EventFeatures) -> f64 {
    // Compute tag similarity using Jaccard index
    let tag_similarity = jaccard_similarity(&features1.tag_patterns, &features2.tag_patterns);

    // Compute content similarity using Jaro-Winkler
    let content_similarity = if features1.normalized_content.is_empty()
        && features2.normalized_content.is_empty()
    {
        1.0 // Both empty = identical
    } else if features1.normalized_content.is_empty() || features2.normalized_content.is_empty() {
        0.0 // One empty, one not = different
    } else {
        strsim::jaro_winkler(&features1.normalized_content, &features2.normalized_content)
    };

    // Weighted combination: tags are more important for structure
    const TAG_WEIGHT: f64 = 0.6;
    const CONTENT_WEIGHT: f64 = 0.4;

    tag_similarity * TAG_WEIGHT + content_similarity * CONTENT_WEIGHT
}

/// Cluster kinds based on similarity threshold using greedy algorithm.
///
/// Returns a map from Kind to (cluster_id, average_similarity).
///
/// Only kinds with at least one similar neighbor (similarity >= threshold) get a cluster_id.
/// Standalone kinds remain unclustered.
///
/// # Algorithm
///
/// 1. Sort kinds for deterministic results
/// 2. For each kind, try to assign it to an existing cluster
/// 3. If no cluster matches (similarity < threshold), track as potential new cluster
/// 4. Remove single-member clusters at the end
/// 5. Compute average similarity for each kind to its cluster members
///
/// # Note
///
/// Currently unused - clustering now uses Qdrant's native similarity search API.
/// Kept for potential future use and testing.
#[allow(dead_code)]
pub fn cluster_kinds(events: &HashMap<Kind, &Event>, threshold: f64) -> HashMap<Kind, (u32, f64)> {
    let mut clusters: HashMap<Kind, u32> = HashMap::new();
    let mut cluster_representatives: Vec<(u32, Kind, EventFeatures)> = Vec::new();

    // Sort kinds for deterministic clustering
    let mut sorted_kinds: Vec<Kind> = events.keys().copied().collect();
    sorted_kinds.sort();

    for kind in sorted_kinds {
        let event = events[&kind];
        let features = EventFeatures::from_event(event);

        // Try to find a cluster this event belongs to
        let mut best_cluster: Option<u32> = None;
        let mut best_similarity: f64 = 0.0;

        for (cluster_id, _rep_kind, rep_features) in &cluster_representatives {
            let similarity = compute_similarity(&features, rep_features);

            if similarity >= threshold && similarity > best_similarity {
                best_similarity = similarity;
                best_cluster = Some(*cluster_id);
            }
        }

        // Assign to best cluster or create new one
        if let Some(cluster_id) = best_cluster {
            clusters.insert(kind, cluster_id);
        } else {
            // Create new cluster with this kind as representative
            // Use the kind number itself as the cluster ID for stability
            let cluster_id = u16::from(kind) as u32;

            clusters.insert(kind, cluster_id);
            cluster_representatives.push((cluster_id, kind, features));
        }
    }

    // Count cluster sizes and remove single-member clusters
    let mut cluster_sizes: HashMap<u32, usize> = HashMap::new();
    for cluster_id in clusters.values() {
        *cluster_sizes.entry(*cluster_id).or_insert(0) += 1;
    }

    // Keep only clusters with 2+ members
    clusters.retain(|_kind, cluster_id| cluster_sizes.get(cluster_id).copied().unwrap_or(0) >= 2);

    // Compute similarity to cluster representative for each kind
    // This is much faster than computing all pairwise similarities
    let mut result: HashMap<Kind, (u32, f64)> = HashMap::new();

    // Build map of cluster_id -> representative features
    let cluster_rep_features: HashMap<u32, &EventFeatures> = cluster_representatives
        .iter()
        .map(|(cid, _kind, features)| (*cid, features))
        .collect();

    for (kind, cluster_id) in clusters {
        let features = EventFeatures::from_event(events[&kind]);
        let rep_features = cluster_rep_features[&cluster_id];

        // Compute similarity to cluster representative
        let similarity = compute_similarity(&features, rep_features);

        result.insert(kind, (cluster_id, similarity));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create test events - use EventBuilder which is simpler
    fn create_test_event(kind: u16, tag_strings: Vec<&str>, content: &str) -> Event {
        let mut builder = EventBuilder::new(Kind::from(kind), content);

        for tag_str in tag_strings {
            let parts: Vec<&str> = tag_str.split(':').collect();
            if parts.len() == 2 {
                builder = builder.tag(Tag::parse([parts[0], parts[1]]).unwrap());
            } else {
                builder = builder.tag(Tag::parse([parts[0]]).unwrap());
            }
        }

        let keys = Keys::generate();
        // For tests, we'll use a synchronous approach by creating the event directly
        // The async signing is only needed for external signers
        let unsigned = builder.build(keys.public_key());

        // Create a simple runtime for the test
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { keys.sign_event(unsigned).await.unwrap() })
    }

    #[test]
    fn test_jaccard_similarity() {
        let set1: HashSet<String> = vec!["a".to_string(), "b".to_string(), "c".to_string()]
            .into_iter()
            .collect();
        let set2: HashSet<String> = vec!["b".to_string(), "c".to_string(), "d".to_string()]
            .into_iter()
            .collect();

        let similarity = jaccard_similarity(&set1, &set2);
        // Intersection: {b, c} = 2 items
        // Union: {a, b, c, d} = 4 items
        // Jaccard: 2/4 = 0.5
        assert_eq!(similarity, 0.5);
    }

    #[test]
    fn test_jaccard_identical() {
        let set1: HashSet<String> = vec!["a".to_string(), "b".to_string()].into_iter().collect();
        let set2 = set1.clone();

        let similarity = jaccard_similarity(&set1, &set2);
        assert_eq!(similarity, 1.0);
    }

    #[test]
    fn test_jaccard_no_overlap() {
        let set1: HashSet<String> = vec!["a".to_string(), "b".to_string()].into_iter().collect();
        let set2: HashSet<String> = vec!["c".to_string(), "d".to_string()].into_iter().collect();

        let similarity = jaccard_similarity(&set1, &set2);
        assert_eq!(similarity, 0.0);
    }

    #[test]
    fn test_compute_similarity_identical_events() {
        // Use 64-char hex IDs so they normalize to <HEX_ID>
        let hex1 = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let hex2 = "0000000000000000000000000000000000000000000000000000000000000001";
        let hex3 = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let hex4 = "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";

        let event1 = create_test_event(
            1000,
            vec![&format!("e:{}", hex1), &format!("p:{}", hex2)],
            "Hello world",
        );
        let event2 = create_test_event(
            1001,
            vec![&format!("e:{}", hex3), &format!("p:{}", hex4)],
            "Hello world",
        );

        let features1 = EventFeatures::from_event(&event1);
        let features2 = EventFeatures::from_event(&event2);

        let similarity = compute_similarity(&features1, &features2);
        // Should be very high since structure is identical (only IDs differ, which normalize to <HEX_ID>)
        assert!(similarity > 0.9, "Similarity was only {}", similarity);
    }

    #[test]
    fn test_compute_similarity_different_events() {
        let event1 = create_test_event(1000, vec!["e:abc", "p:def"], "Hello world");
        let event2 = create_test_event(1001, vec!["d:test", "t:nostr"], "Different content here");

        let features1 = EventFeatures::from_event(&event1);
        let features2 = EventFeatures::from_event(&event2);

        let similarity = compute_similarity(&features1, &features2);
        // Should be low since both tags and content are different
        assert!(similarity < 0.5);
    }

    #[test]
    fn test_cluster_kinds() {
        // Create similar events with normalized hex IDs
        let hex1 = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let hex2 = "0000000000000000000000000000000000000000000000000000000000000001";
        let hex3 = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
        let hex4 = "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";

        let event1 = create_test_event(
            1000,
            vec![&format!("e:{}", hex1), &format!("p:{}", hex2)],
            "Hello world",
        );
        let event2 = create_test_event(
            1001,
            vec![&format!("e:{}", hex3), &format!("p:{}", hex4)],
            "Hello world",
        );
        let event3 = create_test_event(1002, vec!["d:test"], "Completely different");

        let mut events = HashMap::new();
        events.insert(Kind::from(1000), &event1);
        events.insert(Kind::from(1001), &event2);
        events.insert(Kind::from(1002), &event3);

        let clusters = cluster_kinds(&events, 0.7);

        // event1 and event2 should be in the same cluster (same structure, different IDs)
        assert!(
            clusters.contains_key(&Kind::from(1000)),
            "event1 should be clustered"
        );
        assert!(
            clusters.contains_key(&Kind::from(1001)),
            "event2 should be clustered"
        );

        let (cluster_id_1, similarity_1) = clusters[&Kind::from(1000)];
        let (cluster_id_2, similarity_2) = clusters[&Kind::from(1001)];

        assert_eq!(
            cluster_id_1, cluster_id_2,
            "event1 and event2 should be in same cluster"
        );

        // Both should have high similarity scores (>0.9 since they're very similar)
        assert!(
            similarity_1 > 0.9,
            "event1 similarity should be high: {}",
            similarity_1
        );
        assert!(
            similarity_2 > 0.9,
            "event2 similarity should be high: {}",
            similarity_2
        );

        // event3 should NOT be clustered (no similar neighbors)
        assert!(
            !clusters.contains_key(&Kind::from(1002)),
            "event3 should not be clustered (standalone)"
        );
    }
}
