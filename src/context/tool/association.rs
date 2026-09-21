//! An association key must designate exactly one invocation and one result.
//! Retain consumed and ambiguous keys so later events cannot reinterpret an
//! earlier success. These tables live only for the bounded input pass.

use super::AssociatedToolCall;
use crate::context::ToolStatus;
use std::collections::{BTreeMap, btree_map::Entry};

#[derive(Clone, Copy)]
pub(super) enum PendingCall {
    Waiting(usize),
    Completed(usize),
    Ambiguous,
}

pub(super) fn register<K: Ord>(
    pending: &mut BTreeMap<K, PendingCall>,
    key: K,
    index: usize,
    calls: &mut [AssociatedToolCall],
    sequential: bool,
    completed: bool,
) {
    let next = if completed {
        PendingCall::Completed(index)
    } else {
        PendingCall::Waiting(index)
    };
    match pending.entry(key) {
        Entry::Vacant(slot) => {
            slot.insert(next);
        }
        Entry::Occupied(mut slot) => match *slot.get() {
            PendingCall::Completed(_) if sequential => {
                // No-ID matching is explicitly sequential within one turn.
                // Reusing the name after a completed pair is not a collision.
                slot.insert(next);
            }
            PendingCall::Waiting(previous) | PendingCall::Completed(previous) => {
                calls[previous].status = ToolStatus::Unknown;
                calls[index].status = ToolStatus::Unknown;
                slot.insert(PendingCall::Ambiguous);
            }
            PendingCall::Ambiguous => {
                calls[index].status = ToolStatus::Unknown;
            }
        },
    }
}

/// Return the unique invocation, or whether the result has an ambiguous key.
/// An ordinary orphan retains its existing representation; an ambiguous result
/// must not acquire authority merely by carrying a tool name and arguments.
pub(super) fn take<K: Ord>(
    pending: &mut BTreeMap<K, PendingCall>,
    key: &K,
    calls: &mut [AssociatedToolCall],
) -> (Option<usize>, bool) {
    let Some(state) = pending.get_mut(key) else {
        return (None, false);
    };
    match *state {
        PendingCall::Waiting(index) => {
            *state = PendingCall::Completed(index);
            (Some(index), false)
        }
        PendingCall::Completed(index) => {
            // A distinct second result cannot rewrite the outcome or produce
            // a second observation. Exact event redelivery is deduplicated by
            // the caller before reaching this table.
            calls[index].status = ToolStatus::Unknown;
            *state = PendingCall::Ambiguous;
            (None, true)
        }
        PendingCall::Ambiguous => (None, true),
    }
}
