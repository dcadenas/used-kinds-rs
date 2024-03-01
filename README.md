# Tracker for Undocumented Nostr Event Kinds
A service that monitors the Nostr network for event kinds not yet recognized by Nostr Improvement Proposals (NIPs).

**Purpose:**
1. Help developers and researchers find unique identifiers for new kinds, minimizing duplication risks.
2. Offer insight into the network's innovative uses, facilitating awareness of trends and creative applications within the Nostr network.

**Data Treatment:**
- Event kinds not seen for over a month are removed to maintain the dataset's relevance.
- I may add a json endpoint to fetch the data at some point but contributions are welcome.

**Technical Details:**
- **Language:** Developed in Rust, primarily as an exploration of the language's capabilities.
- **License:** Open-source, available under the MIT License.
- **Contributing:** I welcome contributions, including bug fixes, feature proposals, and documentation improvements.
