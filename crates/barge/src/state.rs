//! Account-wide state: the state directory, locking, atomic publish, bounded
//! reads and journal records (stage 1 design, "Account-wide state directory").
//!
//! Slice S2. S1 only projects the resolved state directory path into
//! [`crate::describe`]; nothing here creates, locks or writes yet.
