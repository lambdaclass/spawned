//! `PendingReplies<K, R>`: a bounded table of deferred tokens by key
//! (docs/design/deferred-replies.md §7).

use super::ReplyTo;
use std::collections::HashMap;
use std::hash::Hash;

/// Why [`PendingReplies::insert`] handed the token back. The handler decides
/// the answer (typically `token.overloaded()` or a domain error) and still
/// owns the token.
#[derive(Debug)]
#[non_exhaustive]
pub enum InsertError<R: Send + 'static> {
    /// A live token is already stored under this key; it is kept.
    Duplicate(ReplyTo<R>),
    /// The table holds `cap` live tokens.
    Full(ReplyTo<R>),
}

impl<R: Send + 'static> InsertError<R> {
    /// The token handed back.
    pub fn into_inner(self) -> ReplyTo<R> {
        match self {
            Self::Duplicate(token) | Self::Full(token) => token,
        }
    }
}

/// Deferred tokens by key, capped: retained entries never exceed `cap`,
/// however fast tokens expire. Terminal tokens (answered, expired, abandoned)
/// are purged on every insert and skipped by `take`.
///
/// Experimental in 0.8: a utility shaped by one client (requests by id
/// answered when a peer responds); it may change before 1.0.
pub struct PendingReplies<K, R: Send + 'static> {
    cap: usize,
    tokens: HashMap<K, ReplyTo<R>>,
}

impl<K, R: Send + 'static> std::fmt::Debug for PendingReplies<K, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingReplies")
            .field("cap", &self.cap)
            .field("retained", &self.tokens.len())
            .finish()
    }
}

impl<K: Hash + Eq, R: Send + 'static> PendingReplies<K, R> {
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            cap,
            tokens: HashMap::new(),
        }
    }

    /// Purge terminal tokens, then insert. A duplicate key (with a live
    /// token) or a full table hands the new token back.
    pub fn insert(&mut self, key: K, token: ReplyTo<R>) -> Result<(), InsertError<R>> {
        self.purge();
        if self.tokens.contains_key(&key) {
            return Err(InsertError::Duplicate(token));
        }
        if self.tokens.len() >= self.cap {
            return Err(InsertError::Full(token));
        }
        self.tokens.insert(key, token);
        Ok(())
    }

    /// Remove and return the live token under `key`; a terminal one is
    /// removed and skipped.
    pub fn take(&mut self, key: &K) -> Option<ReplyTo<R>> {
        let token = self.tokens.remove(key)?;
        (!token.is_closed()).then_some(token)
    }

    /// Remove every terminal token.
    fn purge(&mut self) {
        self.tokens.retain(|_, token| !token.is_closed());
    }

    /// Live (non-terminal) tokens held: a scan, so a token whose caller
    /// just left no longer counts.
    pub fn len(&self) -> usize {
        self.tokens.values().filter(|t| !t.is_closed()).count()
    }

    /// Whether no live token is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Take every live token (for example to answer them in `stopped()`).
    pub fn drain(&mut self) -> impl Iterator<Item = (K, ReplyTo<R>)> + '_ {
        self.tokens.drain().filter(|(_, token)| !token.is_closed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reply::{reply_pair, Deadline};
    use crate::ActorError;

    fn token() -> (ReplyTo<u32>, crate::reply::ReplyReceiver<u32>) {
        let (reply, rx) = reply_pair::<u32>(Deadline::Never);
        (reply.defer().unwrap(), rx)
    }

    #[test]
    fn bounded_with_duplicates_and_purge() {
        let mut table = PendingReplies::with_capacity(2);
        let (a, ra) = token();
        let (b, rb) = token();
        let (c, rc) = token();
        let (dup, rdup) = token();
        table.insert(1, a).unwrap();
        table.insert(2, b).unwrap();
        let full = table.insert(3, c).unwrap_err();
        assert!(matches!(full, InsertError::Full(_)));
        full.into_inner().overloaded();
        assert!(matches!(rc.recv(), Err(ActorError::Overloaded)));
        let duplicate = table.insert(1, dup).unwrap_err();
        assert!(matches!(duplicate, InsertError::Duplicate(_)));
        drop(duplicate);
        assert!(matches!(rdup.recv(), Err(ActorError::ReplyDropped)));
        // A caller that left frees its entry on the next insert.
        drop(rb);
        assert_eq!(
            table.tokens.len(),
            2,
            "terminal tokens stay until an insert"
        );
        assert_eq!(table.len(), 1);
        let (d, rd) = token();
        table.insert(4, d).unwrap();
        assert_eq!(table.tokens.len(), 2);
        assert_eq!(table.len(), 2);
        assert!(table.take(&2).is_none());
        table.take(&1).unwrap().reply(10);
        table.take(&4).unwrap().reply(40);
        assert_eq!(ra.recv().unwrap(), 10);
        assert_eq!(rd.recv().unwrap(), 40);
    }
}
