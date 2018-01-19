// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.
/*
/// Distributed Schnorr-signing session.
/// Based on "Efficient Multi-Party Digital Signature using Adaptive Secret Sharing for Low-Power Devices in Wireless Network" paper.
/// Brief overview:
/// 1) initialization: master node (which has received request for signing the message) requests all other nodes to sign the message
/// 2) ACL check: all nodes which have received the request are querying ACL-contract to check if requestor has access to the private key
/// 3) partial signing: every node which has succussfully checked access for the requestor do a partial signing
/// 4) signing: master node receives all partial signatures of the secret and computes the signature
pub struct SessionImpl {
	/// Session core.
	core: SessionCore,
	/// Session data.
	data: Mutex<SessionData>,
}

/// Immutable session data.
struct SessionCore {
	/// Session metadata.
	pub meta: SessionMeta,
	/// Signing session access key.
	pub access_key: Secret,
	/// Key share.
	pub key_share: Option<DocumentKeyShare>,
	/// Cluster which allows this node to send messages to other nodes in the cluster.
	pub cluster: Arc<Cluster>,
	/// Session-level nonce.
	pub nonce: u64,
	/// SessionImpl completion condvar.
	pub completed: Condvar,
}

/// Signing consensus session type.
type SigningConsensusSession = ConsensusSession<KeyAccessJob, SigningConsensusTransport, SigningJob, SigningJobTransport>;

/// Mutable session data.
struct SessionData {
	/// Session state.
	pub state: SessionState,
	/// Message hash.
	pub message_hash: Option<H256>,
	/// Key version to use for decryption.
	pub version: Option<H256>,
	/// Consensus-based signing session.
	pub consensus_session: SigningConsensusSession,
	/// Session key generation session.
	pub generation_session: Option<GenerationSession>,
	/// Delegation status.
	pub delegation_status: Option<DelegationStatus>,
	/// Decryption result.
	pub result: Option<Result<(Secret, Secret), Error>>,
}*/