use crate::normalization::{analyze_json_structure, normalize_content, normalize_tags};
use nostr_sdk::prelude::*;
use std::collections::{HashMap, HashSet};

/// Version of the feature extraction scheme in `EventFeatures::to_vector`.
///
/// Stored in every Qdrant point payload; bump it whenever the vector layout
/// or any feature computation changes so stored points get re-vectorized at
/// startup. Mixing vectors from different schemes makes similarity garbage.
///
/// v3: `max_nesting_depth` is computed for real (it was constantly 0 in v2).
pub const FEATURE_VERSION: i64 = 3;

// 64-dim vector layout. Each family is normalized to unit energy before the
// global normalization so no family can drown the others.
pub const TAG_NAME_DIMS: usize = 20; // 0..20
pub const TAG_PATTERN_DIMS: usize = 10; // 20..30
pub const JSON_KEY_DIMS: usize = 14; // 30..44
pub const ENCODING_DIMS: usize = 4; // 44..48
pub const CONTENT_DIMS: usize = 8; // 48..56
pub const JSON_SHAPE_DIMS: usize = 8; // 56..64

/// FNV-1a (32-bit). Deliberately hand-rolled: bucket assignments persist in
/// Qdrant, and std's DefaultHasher is not stable across Rust releases.
fn fnv1a_32(s: &str) -> u32 {
    let mut hash: u32 = 0x811c9dc5;
    for byte in s.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// Weight for a tag name bucket. Ubiquitous standard single-letter tags
/// appear in most kinds and carry little cluster signal.
fn tag_weight(tag_name: &str) -> f32 {
    match tag_name {
        "e" | "p" | "d" | "a" | "t" => 0.3,
        _ => 1.0,
    }
}

/// Scale a feature family to unit energy so every family gets an equal vote
/// in the cosine; without this a kind with many tags drowns its content.
fn normalize_family(family: &mut [f32]) {
    let magnitude: f32 = family.iter().map(|x| x * x).sum::<f32>().sqrt();
    if magnitude > 0.0 {
        family.iter_mut().for_each(|x| *x /= magnitude);
    }
}

/// Blend a stored vector with a fresh one (exponential moving average) so a
/// kind's fingerprint reflects its typical event, not just the latest one.
/// Returns a unit-length vector.
pub fn blend_vectors(old: &[f32], fresh: &[f32], keep: f32) -> Vec<f32> {
    let mut blended: Vec<f32> = old
        .iter()
        .zip(fresh.iter())
        .map(|(o, f)| keep * o + (1.0 - keep) * f)
        .collect();

    let magnitude: f32 = blended.iter().map(|x| x * x).sum::<f32>().sqrt();
    if magnitude > 0.0 {
        blended.iter_mut().for_each(|x| *x /= magnitude);
    }

    blended
}

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
    /// 64 dims in six families (see the layout constants), each family
    /// normalized to unit energy, then the whole vector normalized for
    /// cosine distance. Bump [`FEATURE_VERSION`] when changing anything here.
    pub fn to_vector(&self) -> Vec<f32> {
        let mut vector = Vec::with_capacity(crate::qdrant_client::vector_size());

        // Tag name and tag pattern banks. The pattern keeps the value
        // structure ("d:<HEX_ID>" vs "d:value"), which the name bank drops.
        let mut tag_names = [0.0f32; TAG_NAME_DIMS];
        let mut tag_patterns = [0.0f32; TAG_PATTERN_DIMS];

        for pattern in &self.tag_patterns {
            let name = pattern.split(':').next().unwrap_or(pattern);
            let weight = tag_weight(name);

            let name_bucket = (fnv1a_32(name) as usize) % TAG_NAME_DIMS;
            tag_names[name_bucket] = tag_names[name_bucket].max(weight);

            let pattern_bucket = (fnv1a_32(pattern) as usize) % TAG_PATTERN_DIMS;
            tag_patterns[pattern_bucket] = tag_patterns[pattern_bucket].max(weight);
        }

        normalize_family(&mut tag_names);
        normalize_family(&mut tag_patterns);
        vector.extend_from_slice(&tag_names);
        vector.extend_from_slice(&tag_patterns);

        // JSON top-level key names: the strongest "same codebase wrote this"
        // signal available without semantics. Shape stats alone cannot tell
        // {"cost","cpu"} from {"name","about"}.
        let json_struct = analyze_json_structure(&self.normalized_content);

        let mut json_keys = [0.0f32; JSON_KEY_DIMS];
        for key in &json_struct.top_level_keys {
            json_keys[(fnv1a_32(key) as usize) % JSON_KEY_DIMS] = 1.0;
        }
        normalize_family(&mut json_keys);
        vector.extend_from_slice(&json_keys);

        // Content encoding profile
        let content = &self.normalized_content;
        let content_len = content.len() as f32;
        let mut encoding = [0.0f32; ENCODING_DIMS];

        if content_len > 0.0 {
            let hex_chars = content.chars().filter(|c| c.is_ascii_hexdigit()).count() as f32;
            let base64_chars = content
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '+' || *c == '/' || *c == '=')
                .count() as f32;
            let alpha_chars = content.chars().filter(|c| c.is_alphabetic()).count() as f32;
            let unique_chars = content.chars().collect::<HashSet<_>>().len() as f32;

            encoding[0] = hex_chars / content_len;
            encoding[1] = base64_chars / content_len;
            encoding[2] = alpha_chars / content_len;
            encoding[3] = unique_chars / content_len;
        }
        normalize_family(&mut encoding);
        vector.extend_from_slice(&encoding);

        // Content structure: size profile plus normalized-placeholder flags
        let mut content_structure = [0.0f32; CONTENT_DIMS];
        content_structure[0] = (content_len / 1000.0).min(1.0);
        content_structure[1] = if content_len == 0.0 { 1.0 } else { 0.0 };
        content_structure[2] = if content_len > 500.0 { 1.0 } else { 0.0 };
        content_structure[3] = if content_len > 100.0 && content_len < 500.0 {
            1.0
        } else {
            0.0
        };
        content_structure[4] = if content.contains("<HEX_ID>") {
            1.0
        } else {
            0.0
        };
        content_structure[5] = if content.contains("<NIP19>") {
            1.0
        } else {
            0.0
        };
        content_structure[6] = if content.contains("<TIMESTAMP>") {
            1.0
        } else {
            0.0
        };
        content_structure[7] = if content.contains("<URL>") { 1.0 } else { 0.0 };
        normalize_family(&mut content_structure);
        vector.extend_from_slice(&content_structure);

        // JSON shape
        let mut json_shape = [0.0f32; JSON_SHAPE_DIMS];
        json_shape[0] = if json_struct.is_valid_json { 1.0 } else { 0.0 };
        json_shape[1] = if json_struct.is_object { 1.0 } else { 0.0 };
        json_shape[2] = if json_struct.is_array { 1.0 } else { 0.0 };
        json_shape[3] = (json_struct.top_level_key_count as f32 / 20.0).min(1.0);
        json_shape[4] = (json_struct.nested_key_count as f32 / 50.0).min(1.0);
        json_shape[5] = (json_struct.max_nesting_depth as f32 / 5.0).min(1.0);
        json_shape[6] = if json_struct.has_arrays { 1.0 } else { 0.0 };
        json_shape[7] = json_struct.string_value_ratio;
        normalize_family(&mut json_shape);
        vector.extend_from_slice(&json_shape);

        debug_assert_eq!(vector.len(), crate::qdrant_client::vector_size());

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

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }

    fn squared_energy(slice: &[f32]) -> f32 {
        slice.iter().map(|x| x * x).sum()
    }

    fn features(tag_patterns: &[&str], content: &str) -> EventFeatures {
        EventFeatures {
            tag_patterns: tag_patterns.iter().map(|s| s.to_string()).collect(),
            normalized_content: content.to_string(),
        }
    }

    #[test]
    fn test_fnv1a_distinguishes_anagrams() {
        assert_ne!(fnv1a_32("ab"), fnv1a_32("ba"));
        assert_ne!(fnv1a_32("listen"), fnv1a_32("silent"));
        // FNV-1a offset basis for the empty string; pins the algorithm.
        assert_eq!(fnv1a_32(""), 0x811c9dc5);
    }

    #[test]
    fn test_tag_weight_downweights_standard_single_letter_tags() {
        for standard in ["e", "p", "d", "a", "t"] {
            assert!(
                tag_weight(standard) < 0.5,
                "tag {} should be light",
                standard
            );
        }
        assert_eq!(tag_weight("imeta"), 1.0);
        assert_eq!(tag_weight("emoji"), 1.0);
    }

    #[test]
    fn test_to_vector_is_64_dims_and_unit_length() {
        let f = features(&["e:<HEX_ID>", "imeta:<LONG_VALUE>"], r#"{"a":1}"#);
        let v = f.to_vector();
        assert_eq!(v.len(), 64);
        let magnitude: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((magnitude - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_json_key_names_discriminate_same_shape_payloads() {
        // Identical shape, value types, lengths, and char multisets (anagram
        // keys) — only the key names differ. These must not look identical.
        let f1 = features(&[], r#"{"listen":1,"stream":"x"}"#);
        let f2 = features(&[], r#"{"silent":1,"master":"x"}"#);

        let similarity = cosine(&f1.to_vector(), &f2.to_vector());
        assert!(
            similarity < 0.95,
            "different key vocabularies should not be near-identical, got {}",
            similarity
        );

        // Same keys → near identical.
        let f3 = features(&[], r#"{"listen":2,"stream":"y"}"#);
        let same = cosine(&f1.to_vector(), &f3.to_vector());
        assert!(
            same > 0.99,
            "same key vocabulary should match, got {}",
            same
        );
    }

    #[test]
    fn test_tag_value_types_distinguish_kinds() {
        let f1 = features(&["d:<HEX_ID>"], "");
        let f2 = features(&["d:value"], "");

        let similarity = cosine(&f1.to_vector(), &f2.to_vector());
        assert!(
            similarity < 0.999,
            "same tag name with different value structure should differ, got {}",
            similarity
        );
    }

    #[test]
    fn test_family_energies_are_balanced() {
        // Many tag types must not drown the JSON shape family.
        let f = features(
            &[
                "imeta:<URL>",
                "emoji:value",
                "title:value",
                "alt:value",
                "subject:value",
                "client:value",
                "expiration:<TIMESTAMP>",
                "relays:value",
            ],
            r#"{"a":1,"b":"x","c":true}"#,
        );
        let v = f.to_vector();

        let tag_energy = squared_energy(&v[0..TAG_NAME_DIMS]);
        let shape_energy = squared_energy(
            &v[TAG_NAME_DIMS + TAG_PATTERN_DIMS + JSON_KEY_DIMS + ENCODING_DIMS + CONTENT_DIMS..],
        );

        assert!(tag_energy > 0.0 && shape_energy > 0.0);
        let ratio = tag_energy / shape_energy;
        assert!(
            (0.66..1.5).contains(&ratio),
            "families should carry comparable energy, ratio {}",
            ratio
        );
    }

    #[test]
    fn test_blend_vectors_leans_toward_kept_history() {
        let old = vec![1.0, 0.0];
        let fresh = vec![0.0, 1.0];
        let blended = blend_vectors(&old, &fresh, 0.8);

        let magnitude: f32 = blended.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (magnitude - 1.0).abs() < 1e-5,
            "blend must stay unit length"
        );
        assert!(
            cosine(&blended, &old) > cosine(&blended, &fresh),
            "keep=0.8 should lean toward the old vector"
        );
    }

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
