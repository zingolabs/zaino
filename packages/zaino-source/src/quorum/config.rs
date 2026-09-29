//! The configuration section a quorum is built from.

use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};

use super::{Quorum, QuorumConfigError};

/// The members of a quorum and how many of them must agree.
///
/// Owned here, beside the wrapper, so every composition root that assembles a
/// validator set from configuration reads the same section. `M` is the
/// per-member endpoint configuration, whatever the adapter in use takes; this
/// section knows only that there is a list of them.
///
/// ```toml
/// quorum = 2
///
/// [[members]]
/// jsonrpc_address = "zebra-a:8232"
///
/// [[members]]
/// jsonrpc_address = "zebra-b:8232"
///
/// [[members]]
/// jsonrpc_address = "zebra-c:8232"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct QuorumConfig<M> {
    /// The validators, in the order fetches are spread over them.
    pub members: Vec<M>,
    /// How many members must report the same tip for it to be the source's
    /// tip. Absent: a majority, which is `1` for a single member.
    #[serde(default)]
    pub quorum: Option<NonZeroUsize>,
}

impl<M> QuorumConfig<M> {
    /// A quorum of `members` agreeing by majority.
    pub fn majority(members: Vec<M>) -> Self {
        Self {
            members,
            quorum: None,
        }
    }

    /// The agreement size in force: the configured one, else a majority —
    /// strictly more than half the members.
    pub fn quorum(&self) -> NonZeroUsize {
        self.quorum
            .unwrap_or_else(|| NonZeroUsize::MIN.saturating_add(self.members.len() / 2))
    }

    /// The section names at least one member and a quorum it can reach.
    pub fn validate(&self) -> Result<(), QuorumConfigError> {
        if self.members.is_empty() {
            return Err(QuorumConfigError::NoMembers);
        }
        let quorum = self.quorum().get();
        if quorum > self.members.len() {
            return Err(QuorumConfigError::QuorumExceedsMembers {
                quorum,
                members: self.members.len(),
            });
        }
        Ok(())
    }

    /// Connect every member through `connect` and assemble the quorum.
    ///
    /// The section is validated first, so a member is never connected for a
    /// quorum that could not be built. A member that fails to connect fails
    /// the build: a quorum that silently started short would report a
    /// different agreement than the one configured.
    pub fn build<A, E>(
        &self,
        connect: impl FnMut(&M) -> Result<A, E>,
    ) -> Result<Quorum<A>, QuorumBuildError<E>> {
        self.validate().map_err(QuorumBuildError::Config)?;
        let members = self
            .members
            .iter()
            .map(connect)
            .collect::<Result<Vec<A>, E>>()
            .map_err(QuorumBuildError::Connect)?;
        Quorum::new(members, self.quorum()).map_err(QuorumBuildError::Config)
    }
}

/// A quorum could not be built from its configuration.
#[derive(Debug, thiserror::Error)]
pub enum QuorumBuildError<E> {
    /// The section itself is unusable.
    #[error("quorum configuration")]
    Config(#[source] QuorumConfigError),
    /// A member could not be connected.
    #[error("connecting a quorum member")]
    Connect(#[source] E),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn majority_is_one_more_than_half() {
        let of = |n: usize| QuorumConfig::majority(vec![(); n]).quorum().get();
        assert_eq!(of(1), 1);
        assert_eq!(of(2), 2);
        assert_eq!(of(3), 2);
        assert_eq!(of(4), 3);
        assert_eq!(of(5), 3);
    }

    #[test]
    fn validation_rejects_no_members_and_an_unreachable_quorum() {
        assert_eq!(
            QuorumConfig::<()>::majority(vec![]).validate(),
            Err(QuorumConfigError::NoMembers)
        );
        let too_many = QuorumConfig {
            members: vec![(), ()],
            quorum: NonZeroUsize::new(3),
        };
        assert_eq!(
            too_many.validate(),
            Err(QuorumConfigError::QuorumExceedsMembers {
                quorum: 3,
                members: 2
            })
        );
        assert_eq!(QuorumConfig::majority(vec![(), ()]).validate(), Ok(()));
    }

    #[test]
    fn a_member_that_fails_to_connect_fails_the_build() {
        let config = QuorumConfig::majority(vec![1u8, 2, 3]);
        let built = config.build(|member| {
            if *member == 2 {
                Err("member two is down")
            } else {
                Ok(*member)
            }
        });
        assert!(matches!(
            built,
            Err(QuorumBuildError::Connect("member two is down"))
        ));

        let quorum = config
            .build(|member| Ok::<u8, &str>(*member))
            .expect("all members connect");
        assert_eq!(quorum.members(), 3);
        assert_eq!(quorum.quorum().get(), 2);
    }

    #[test]
    fn the_section_round_trips_through_toml() {
        let toml = r#"
quorum = 2

[[members]]
address = "a"

[[members]]
address = "b"

[[members]]
address = "c"
"#;
        #[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
        struct Member {
            address: String,
        }
        let parsed: QuorumConfig<Member> = toml::from_str(toml).expect("parses");
        assert_eq!(parsed.members.len(), 3);
        assert_eq!(parsed.quorum().get(), 2);

        let without_quorum: QuorumConfig<Member> =
            toml::from_str("[[members]]\naddress = \"a\"\n").expect("parses");
        assert_eq!(without_quorum.quorum().get(), 1);

        assert!(toml::from_str::<QuorumConfig<Member>>("members = []\nbogus = 1\n").is_err());
    }
}
