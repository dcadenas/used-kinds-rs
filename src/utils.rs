// A static list of taken kinds, with comments indicating the purpose of each kind.
// These kinds are based on the Nostr protocol documentation at https://github.com/nostr-protocol/nips/edit/master/README.md
static TAKEN_KINDS: [u32; 77] = [
    0,    // Metadata
    1,    // Short Text Note
    2,    // Recommend Relay (deprecated)
    3,    // Follows
    4,    // Encrypted Direct Messages
    5,    // Event Deletion
    6,    // Repost
    7,    // Reaction
    8,    // Badge Award
    13,   // Seal
    16,   // Generic Repost
    40,   // Channel Creation
    41,   // Channel Metadata
    42,   // Channel Message
    43,   // Channel Hide Message
    44,   // Channel Mute User
    1021, // Bid
    1022, // Bid confirmation
    1040, // OpenTimestamps
    1059, // Gift Wrap
    1063, // File Metadata
    1311, // Live Chat Message
    1971, // Problem Tracker
    1984, // Reporting
    1985, // Label
    4550, // Community Post Approval
    // The range 5000-5999 is reserved for Job Requests
    // The range 6000-6999 is reserved for Job Results
    7000,  // Job Feedback
    9041,  // Zap Goal
    9734,  // Zap Request
    9735,  // Zap
    9802,  // Highlights
    10000, // Mute list
    10001, // Pin list
    10002, // Relay List Metadata
    10003, // Bookmark list
    10004, // Communities list
    10005, // Public chats list
    10006, // Blocked relays list
    10007, // Search relays list
    10015, // Interests list
    10030, // User emoji list
    10096, // File storage server list
    13194, // Wallet Info
    21000, // Lightning Pub RPC
    22242, // Client Authentication
    23194, // Wallet Request
    23195, // Wallet Response
    24133, // Nostr Connect
    27235, // HTTP Auth
    30000, // Follow sets
    30001, // Generic lists
    30002, // Relay sets
    30003, // Bookmark sets
    30004, // Curation sets
    30008, // Profile Badges
    30009, // Badge Definition
    30015, // Interest sets
    30017, // Create or update a stall
    30018, // Create or update a product
    30019, // Marketplace UI/UX
    30020, // Product sold as an auction
    30023, // Long-form Content
    30024, // Draft Long-form Content
    30030, // Emoji sets
    30063, // Release artifact sets
    30078, // Application-specific Data
    30311, // Live Event
    30315, // User Statuses
    30402, // Classified Listing
    30403, // Draft Classified Listing
    31922, // Date-Based Calendar Event
    31923, // Time-Based Calendar Event
    31924, // Calendar
    31925, // Calendar Event RSVP
    31989, // Handler recommendation
    31990, // Handler information
    34550, // Community Definition
];

/// Checks if a kind is free (not taken) according to Nostr NIPs.
///
/// # Arguments
///
/// * `kind` - The kind of the event to check.
///
/// # Returns
///
/// Returns `true` if the kind is free; otherwise, returns `false`.
pub fn is_kind_free(kind: u32) -> bool {
    !TAKEN_KINDS.contains(&kind) && !(kind >= 5000 && kind <= 6999)
}
