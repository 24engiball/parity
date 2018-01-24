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

use std::collections::{BTreeSet, BTreeMap};
use std::collections::btree_map::Entry;
use std::sync::Arc;
use parking_lot::{Mutex, Condvar};
use ethkey::{Public, Secret, Signature};
use ethereum_types::H256;
use key_server_cluster::{Error, NodeId, SessionId, SessionMeta, AclStorage, DocumentKeyShare};
use key_server_cluster::cluster::{Cluster};
use key_server_cluster::cluster_sessions::{SessionIdWithSubSession, ClusterSession};
use key_server_cluster::generation_session::{SessionImpl as GenerationSession, SessionParams as GenerationSessionParams,
	SessionState as GenerationSessionState};
use key_server_cluster::math;
use key_server_cluster::message::{Message, EcdsaSigningMessage, EcdsaSigningConsensusMessage, EcdsaSignatureNonceGenerationMessage,
	EcdsaInversionNonceGenerationMessage, EcdsaInversionZeroGenerationMessage, EcdsaSigningInversedNonceCoeffShare,
	EcdsaRequestPartialSignature, EcdsaPartialSignature, EcdsaSigningSessionCompleted, GenerationMessage,
	ConsensusMessage, EcdsaSigningSessionError, InitializeConsensusSession, ConfirmConsensusInitialization,
	EcdsaSigningSessionDelegation, EcdsaSigningSessionDelegationCompleted};
use key_server_cluster::jobs::job_session::JobTransport;
use key_server_cluster::jobs::key_access_job::KeyAccessJob;
use key_server_cluster::jobs::signing_job_ecdsa::{EcdsaPartialSigningRequest, EcdsaPartialSigningResponse, EcdsaSigningJob};
use key_server_cluster::jobs::consensus_session::{ConsensusSessionParams, ConsensusSessionState, ConsensusSession};

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
type SigningConsensusSession = ConsensusSession<KeyAccessJob, SigningConsensusTransport, EcdsaSigningJob, SigningJobTransport>;

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
	/// Signature nonce generation session.
	pub sig_nonce_generation_session: Option<GenerationSession>,
	/// Inversion nonce generation session.
	pub inv_nonce_generation_session: Option<GenerationSession>,
	/// Inversion zero generation session.
	pub inv_zero_generation_session: Option<GenerationSession>,
	/// Inversed nonce coefficient shares.
	pub inversed_nonce_coeff_shares: Option<BTreeMap<NodeId, Secret>>,
	/// Delegation status.
	pub delegation_status: Option<DelegationStatus>,
	/// Decryption result.
	pub result: Option<Result<Signature, Error>>,
}

/// Signing session state.
#[derive(Debug, PartialEq)]
pub enum SessionState {
	/// Consensus is establishing.
	ConsensusEstablishing,
	/// Nonces (signature, inversion && zero) are generating.
	NoncesGenerating,
	/// Waiting for inversed nonce shares.
	WaitingForInversedNonceShares,
	/// State when signature is computing.
	SignatureComputing,
	/// Session is completed.
	Finished,
}

/// Session creation parameters
pub struct SessionParams {
	/// Session metadata.
	pub meta: SessionMeta,
	/// Session access key.
	pub access_key: Secret,
	/// Key share.
	pub key_share: Option<DocumentKeyShare>,
	/// ACL storage.
	pub acl_storage: Arc<AclStorage>,
	/// Cluster
	pub cluster: Arc<Cluster>,
	/// Session nonce.
	pub nonce: u64,
}

/// Signing consensus transport.
struct SigningConsensusTransport {
	/// Session id.
	id: SessionId,
	/// Session access key.
	access_key: Secret,
	/// Session-level nonce.
	nonce: u64,
	/// Selected key version (on master node).
	version: Option<H256>,
	/// Cluster.
	cluster: Arc<Cluster>,
}

/// Signing key generation transport.
struct NonceGenerationTransport<F: Fn(GenerationMessage) -> EcdsaSigningMessage + Send + Sync> {
	/// Session access key.
	access_key: Secret,
	/// Cluster.
	cluster: Arc<Cluster>,
	/// Session-level nonce.
	nonce: u64,
	/// Other nodes ids.
	other_nodes_ids: BTreeSet<NodeId>,
	/// Message mapping function.
	map: F,
}

/// Signing job transport
struct SigningJobTransport {
	/// Session id.
	id: SessionId,
	/// Session access key.
	access_key: Secret,
	/// Session-level nonce.
	nonce: u64,
	/// Cluster.
	cluster: Arc<Cluster>,
}

/// Session delegation status.
enum DelegationStatus {
	/// Delegated to other node.
	DelegatedTo(NodeId),
	/// Delegated from other node.
	DelegatedFrom(NodeId, u64),
}

impl SessionImpl {
	/// Create new signing session.
	pub fn new(params: SessionParams, requester_signature: Option<Signature>) -> Result<Self, Error> {
		debug_assert_eq!(params.meta.threshold, params.key_share.as_ref().map(|ks| ks.threshold).unwrap_or_default());

		let consensus_transport = SigningConsensusTransport {
			id: params.meta.id.clone(),
			access_key: params.access_key.clone(),
			nonce: params.nonce,
			version: None,
			cluster: params.cluster.clone(),
		};
		let consensus_session = ConsensusSession::new(ConsensusSessionParams {
			// this session requires responses from 2 * t nodes
			meta: SessionMeta {
				id: params.meta.id,
				master_node_id: params.meta.master_node_id,
				self_node_id: params.meta.self_node_id,
				threshold: params.meta.threshold * 2,
			},
			consensus_executor: match requester_signature {
				Some(requester_signature) => KeyAccessJob::new_on_master(params.meta.id.clone(), params.acl_storage.clone(), requester_signature),
				None => KeyAccessJob::new_on_slave(params.meta.id.clone(), params.acl_storage.clone()),
			},
			consensus_transport: consensus_transport,
		})?;

		Ok(SessionImpl {
			core: SessionCore {
				meta: params.meta,
				access_key: params.access_key,
				key_share: params.key_share,
				cluster: params.cluster,
				nonce: params.nonce,
				completed: Condvar::new(),
			},
			data: Mutex::new(SessionData {
				state: SessionState::ConsensusEstablishing,
				message_hash: None,
				version: None,
				consensus_session: consensus_session,
				sig_nonce_generation_session: None,
				inv_nonce_generation_session: None,
				inv_zero_generation_session: None,
				inversed_nonce_coeff_shares: None,
				delegation_status: None,
				result: None,
			}),
		})
	}

	/// Wait for session completion.
	pub fn wait(&self) -> Result<Signature, Error> {
		Self::wait_session(&self.core.completed, &self.data, None, |data| data.result.clone())
	}

	/// Delegate session to other node.
	pub fn delegate(&self, master: NodeId, version: H256, message_hash: H256) -> Result<(), Error> {
		if self.core.meta.master_node_id != self.core.meta.self_node_id {
			return Err(Error::InvalidStateForRequest);
		}

		let mut data = self.data.lock();
		if data.consensus_session.state() != ConsensusSessionState::WaitingForInitialization || data.delegation_status.is_some() {
			return Err(Error::InvalidStateForRequest);
		}

		data.consensus_session.consensus_job_mut().executor_mut().set_has_key_share(false);
		self.core.cluster.send(&master, Message::EcdsaSigning(EcdsaSigningMessage::EcdsaSigningSessionDelegation(EcdsaSigningSessionDelegation {
			session: self.core.meta.id.clone().into(),
			sub_session: self.core.access_key.clone().into(),
			session_nonce: self.core.nonce,
			requestor_signature: data.consensus_session.consensus_job().executor().requester_signature()
				.expect("signature is passed to master node on creation; session can be delegated from master node only; qed")
				.clone().into(),
			version: version.into(),
			message_hash: message_hash.into(),
		})))?;
		data.delegation_status = Some(DelegationStatus::DelegatedTo(master));
		Ok(())

	}

	/// Initialize signing session on master node.
	pub fn initialize(&self, version: H256, message_hash: H256) -> Result<(), Error> {
		debug_assert_eq!(self.core.meta.self_node_id, self.core.meta.master_node_id);

		// check if version exists
		let key_version = match self.core.key_share.as_ref() {
			None => return Err(Error::InvalidMessage),
			Some(key_share) => key_share.version(&version).map_err(|e| Error::KeyStorage(e.into()))?,
		};

		let mut data = self.data.lock();
		let non_isolated_nodes = self.core.cluster.nodes();
		let mut consensus_nodes: BTreeSet<_> = key_version.id_numbers.keys()
			.filter(|n| non_isolated_nodes.contains(*n))
			.cloned()
			.chain(::std::iter::once(self.core.meta.self_node_id.clone()))
			.collect();
		if let Some(&DelegationStatus::DelegatedFrom(delegation_master, _)) = data.delegation_status.as_ref() {
			consensus_nodes.remove(&delegation_master);
		}

		data.consensus_session.consensus_job_mut().transport_mut().version = Some(version.clone());
		data.version = Some(version.clone());
		data.message_hash = Some(message_hash);
		data.consensus_session.initialize(consensus_nodes)?;

		if data.consensus_session.state() == ConsensusSessionState::ConsensusEstablished {
			/*let generation_session = GenerationSession::new(GenerationSessionParams {
				id: self.core.meta.id.clone(),
				self_node_id: self.core.meta.self_node_id.clone(),
				key_storage: None,
				cluster: Arc::new(SessionKeyGenerationTransport {
					access_key: self.core.access_key.clone(),
					cluster: self.core.cluster.clone(),
					nonce: self.core.nonce,
					other_nodes_ids: BTreeSet::new()
				}),
				nonce: None,
			});
			generation_session.initialize(Public::default(), 0, vec![self.core.meta.self_node_id.clone()].into_iter().collect())?;

			debug_assert_eq!(generation_session.state(), GenerationSessionState::WaitingForGenerationConfirmation);
			let joint_public_and_secret = generation_session
				.joint_public_and_secret()
				.expect("session key is generated before signature is computed; we are in SignatureComputing state; qed")?;
			data.generation_session = Some(generation_session);
			data.state = SessionState::SignatureComputing;

			self.core.disseminate_jobs(&mut data.consensus_session, &version, joint_public_and_secret.0, joint_public_and_secret.1, message_hash)?;

			debug_assert!(data.consensus_session.state() == ConsensusSessionState::Finished);
			let result = data.consensus_session.result()?;
			Self::set_signing_result(&self.core, &mut *data, Ok(result));*/
		}

		Ok(())
	}

	/// Process signing message.
	pub fn process_message(&self, sender: &NodeId, message: &EcdsaSigningMessage) -> Result<(), Error> {
		if self.core.nonce != message.session_nonce() {
			return Err(Error::ReplayProtection);
		}

		match message {
			&EcdsaSigningMessage::EcdsaSigningConsensusMessage(ref message) =>
				self.on_consensus_message(sender, message),
			&EcdsaSigningMessage::EcdsaSignatureNonceGenerationMessage(ref message) =>
				self.on_signature_nonce_generation_message(sender, message),
			&EcdsaSigningMessage::EcdsaInversionNonceGenerationMessage(ref message) =>
				self.on_inversion_nonce_generation_message(sender, message),
			&EcdsaSigningMessage::EcdsaInversionZeroGenerationMessage(ref message) =>
				self.on_inversion_zero_generation_message(sender, message),
			&EcdsaSigningMessage::EcdsaSigningInversedNonceCoeffShare(ref message) =>
				self.on_inversed_nonce_coeff_share(sender, message),
			&EcdsaSigningMessage::EcdsaRequestPartialSignature(ref message) =>
				self.on_partial_signature_requested(sender, message),
			&EcdsaSigningMessage::EcdsaPartialSignature(ref message) =>
				self.on_partial_signature(sender, message),
			&EcdsaSigningMessage::EcdsaSigningSessionError(ref message) =>
				self.process_node_error(Some(&sender), Error::Io(message.error.clone())),
			&EcdsaSigningMessage::EcdsaSigningSessionCompleted(ref message) =>
				self.on_session_completed(sender, message),
			&EcdsaSigningMessage::EcdsaSigningSessionDelegation(ref message) =>
				self.on_session_delegated(sender, message),
			&EcdsaSigningMessage::EcdsaSigningSessionDelegationCompleted(ref message) =>
				self.on_session_delegation_completed(sender, message),
		}
	}

	/// When session is delegated to this node.
	pub fn on_session_delegated(&self, sender: &NodeId, message: &EcdsaSigningSessionDelegation) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);

		{
			let mut data = self.data.lock();
			if data.consensus_session.state() != ConsensusSessionState::WaitingForInitialization || data.delegation_status.is_some() {
				return Err(Error::InvalidStateForRequest);
			}

			data.consensus_session.consensus_job_mut().executor_mut().set_requester_signature(message.requestor_signature.clone().into());
			data.delegation_status = Some(DelegationStatus::DelegatedFrom(sender.clone(), message.session_nonce));
		}

		self.initialize(message.version.clone().into(), message.message_hash.clone().into())
	}

	/// When delegated session is completed on other node.
	pub fn on_session_delegation_completed(&self, sender: &NodeId, message: &EcdsaSigningSessionDelegationCompleted) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);

		if self.core.meta.master_node_id != self.core.meta.self_node_id {
			return Err(Error::InvalidStateForRequest);
		}

		let mut data = self.data.lock();
		match data.delegation_status.as_ref() {
			Some(&DelegationStatus::DelegatedTo(ref node)) if node == sender => (),
			_ => return Err(Error::InvalidMessage),
		}

		Self::set_signing_result(&self.core, &mut *data, Ok(message.signature.clone().into()));

		Ok(())
	}

	/// When consensus-related message is received.
	pub fn on_consensus_message(&self, sender: &NodeId, message: &EcdsaSigningConsensusMessage) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		let mut data = self.data.lock();
		let is_establishing_consensus = data.consensus_session.state() == ConsensusSessionState::EstablishingConsensus;

		if let &ConsensusMessage::InitializeConsensusSession(ref msg) = &message.message {
			let version = msg.version.clone().into();
			let has_key_share = self.core.key_share.as_ref()
				.map(|ks| ks.version(&version).is_ok())
				.unwrap_or(false);
			data.consensus_session.consensus_job_mut().executor_mut().set_has_key_share(has_key_share);
			data.version = Some(version);
		}
		data.consensus_session.on_consensus_message(&sender, &message.message)?;

		let is_consensus_established = data.consensus_session.state() == ConsensusSessionState::ConsensusEstablished;
		if self.core.meta.self_node_id != self.core.meta.master_node_id || !is_establishing_consensus || !is_consensus_established {
			return Ok(());
		}

		let consensus_group = data.consensus_session.select_consensus_group()?.clone();
		let mut other_consensus_group_nodes = consensus_group.clone();
		other_consensus_group_nodes.remove(&self.core.meta.self_node_id);

		let key_share = match self.core.key_share.as_ref() {
			None => return Err(Error::InvalidMessage),
			Some(key_share) => key_share,
		};

		let nonce = self.core.nonce;
		let access_key = self.core.access_key.clone();
		let generation_session = GenerationSession::new(GenerationSessionParams {
			id: self.core.meta.id.clone(),
			self_node_id: self.core.meta.self_node_id.clone(),
			key_storage: None,
			cluster: Arc::new(NonceGenerationTransport {
				access_key: self.core.access_key.clone(),
				cluster: self.core.cluster.clone(),
				nonce: self.core.nonce,
				other_nodes_ids: other_consensus_group_nodes.clone(),
				map: move |m| EcdsaSigningMessage::EcdsaSignatureNonceGenerationMessage(EcdsaSignatureNonceGenerationMessage {
					session: m.session_id().clone().into(),
					sub_session: access_key.clone().into(),
					session_nonce: nonce,
					message: m,
				}),
			}),
			nonce: None,
		});
		generation_session.initialize(Public::default(), false, key_share.threshold * 2, consensus_group.clone())?;
		data.sig_nonce_generation_session = Some(generation_session);

		let nonce = self.core.nonce;
		let access_key = self.core.access_key.clone();
		let generation_session = GenerationSession::new(GenerationSessionParams {
			id: self.core.meta.id.clone(),
			self_node_id: self.core.meta.self_node_id.clone(),
			key_storage: None,
			cluster: Arc::new(NonceGenerationTransport {
				access_key: self.core.access_key.clone(),
				cluster: self.core.cluster.clone(),
				nonce: self.core.nonce,
				other_nodes_ids: other_consensus_group_nodes.clone(),
				map: move |m| EcdsaSigningMessage::EcdsaInversionNonceGenerationMessage(EcdsaInversionNonceGenerationMessage {
					session: m.session_id().clone().into(),
					sub_session: access_key.clone().into(),
					session_nonce: nonce,
					message: m,
				}),
			}),
			nonce: None,
		});
		generation_session.initialize(Public::default(), false, key_share.threshold * 2, consensus_group.clone())?;
		data.inv_nonce_generation_session = Some(generation_session);

		let nonce = self.core.nonce;
		let access_key = self.core.access_key.clone();
		let generation_session = GenerationSession::new(GenerationSessionParams {
			id: self.core.meta.id.clone(),
			self_node_id: self.core.meta.self_node_id.clone(),
			key_storage: None,
			cluster: Arc::new(NonceGenerationTransport {
				access_key: self.core.access_key.clone(),
				cluster: self.core.cluster.clone(),
				nonce: self.core.nonce,
				other_nodes_ids: other_consensus_group_nodes,
				map: move |m| EcdsaSigningMessage::EcdsaInversionZeroGenerationMessage(EcdsaInversionZeroGenerationMessage {
					session: m.session_id().clone().into(),
					sub_session: access_key.clone().into(),
					session_nonce: nonce,
					message: m,
				}),
			}),
			nonce: None,
		});
		generation_session.initialize(Public::default(), true, key_share.threshold * 2, consensus_group)?;
		data.inv_zero_generation_session = Some(generation_session);

		data.state = SessionState::NoncesGenerating;

		Ok(())
	}

	/// When signature nonce generation message is received.
	pub fn on_signature_nonce_generation_message(&self, sender: &NodeId, message: &EcdsaSignatureNonceGenerationMessage) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		let mut data = self.data.lock();

		if let &GenerationMessage::InitializeSession(ref message) = &message.message {
			if &self.core.meta.master_node_id != sender {
				match data.delegation_status.as_ref() {
					Some(&DelegationStatus::DelegatedTo(s)) if s == *sender => (),
					_ => return Err(Error::InvalidMessage),
				}
			}

			let consensus_group: BTreeSet<NodeId> = message.nodes.keys().cloned().map(Into::into).collect();
			let mut other_consensus_group_nodes = consensus_group.clone();
			other_consensus_group_nodes.remove(&self.core.meta.self_node_id);

			let access_key = self.core.access_key.clone();
			let nonce = self.core.nonce;
			let generation_session = GenerationSession::new(GenerationSessionParams {
				id: self.core.meta.id.clone(),
				self_node_id: self.core.meta.self_node_id.clone(),
				key_storage: None,
				cluster: Arc::new(NonceGenerationTransport {
					access_key: self.core.access_key.clone(),
					cluster: self.core.cluster.clone(),
					nonce: self.core.nonce,
					other_nodes_ids: other_consensus_group_nodes,
					map: move |m| EcdsaSigningMessage::EcdsaSignatureNonceGenerationMessage(EcdsaSignatureNonceGenerationMessage {
						session: m.session_id().clone().into(),
						sub_session: access_key.clone().into(),
						session_nonce: nonce,
						message: m,
					}),
				}),
				nonce: None,
			});
			data.sig_nonce_generation_session = Some(generation_session);
			data.state = SessionState::NoncesGenerating;
		}

		{
			let generation_session = data.sig_nonce_generation_session.as_ref().ok_or(Error::InvalidStateForRequest)?;
			let is_key_generating = generation_session.state() != GenerationSessionState::Finished;
			generation_session.process_message(sender, &message.message)?;

			let is_key_generated = generation_session.state() == GenerationSessionState::Finished;
			if !is_key_generating || !is_key_generated {
				return Ok(());
			}
		}

		if !Self::check_nonces_generated(&*data) {
			return Ok(());
		}

		Self::send_inversed_nonce_coeff_share(&self.core, &mut*data)?;
		data.state = if self.core.meta.master_node_id != self.core.meta.self_node_id {
			SessionState::SignatureComputing
		} else {
			SessionState::WaitingForInversedNonceShares
		};

		Ok(())
	}

	/// When inversion nonce generation message is received.
	pub fn on_inversion_nonce_generation_message(&self, sender: &NodeId, message: &EcdsaInversionNonceGenerationMessage) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		let mut data = self.data.lock();

		if let &GenerationMessage::InitializeSession(ref message) = &message.message {
			if &self.core.meta.master_node_id != sender {
				match data.delegation_status.as_ref() {
					Some(&DelegationStatus::DelegatedTo(s)) if s == *sender => (),
					_ => return Err(Error::InvalidMessage),
				}
			}

			let consensus_group: BTreeSet<NodeId> = message.nodes.keys().cloned().map(Into::into).collect();
			let mut other_consensus_group_nodes = consensus_group.clone();
			other_consensus_group_nodes.remove(&self.core.meta.self_node_id);

			let access_key = self.core.access_key.clone();
			let nonce = self.core.nonce;
			let generation_session = GenerationSession::new(GenerationSessionParams {
				id: self.core.meta.id.clone(),
				self_node_id: self.core.meta.self_node_id.clone(),
				key_storage: None,
				cluster: Arc::new(NonceGenerationTransport {
					access_key: self.core.access_key.clone(),
					cluster: self.core.cluster.clone(),
					nonce: self.core.nonce,
					other_nodes_ids: other_consensus_group_nodes,
					map: move |m| EcdsaSigningMessage::EcdsaInversionNonceGenerationMessage(EcdsaInversionNonceGenerationMessage {
						session: m.session_id().clone().into(),
						sub_session: access_key.clone().into(),
						session_nonce: nonce,
						message: m,
					}),
				}),
				nonce: None,
			});
			data.inv_nonce_generation_session = Some(generation_session);
			data.state = SessionState::NoncesGenerating;
		}

		{
			let generation_session = data.inv_nonce_generation_session.as_ref().ok_or(Error::InvalidStateForRequest)?;
			let is_key_generating = generation_session.state() != GenerationSessionState::Finished;
			generation_session.process_message(sender, &message.message)?;

			let is_key_generated = generation_session.state() == GenerationSessionState::Finished;
			if !is_key_generating || !is_key_generated {
				return Ok(());
			}
		}

		if !Self::check_nonces_generated(&*data) {
			return Ok(());
		}

		Self::send_inversed_nonce_coeff_share(&self.core, &mut*data)?;
		data.state = if self.core.meta.master_node_id != self.core.meta.self_node_id {
			SessionState::SignatureComputing
		} else {
			SessionState::WaitingForInversedNonceShares
		};

		Ok(())
	}

	/// When inversion zero generation message is received.
	pub fn on_inversion_zero_generation_message(&self, sender: &NodeId, message: &EcdsaInversionZeroGenerationMessage) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		let mut data = self.data.lock();

		if let &GenerationMessage::InitializeSession(ref message) = &message.message {
			if &self.core.meta.master_node_id != sender {
				match data.delegation_status.as_ref() {
					Some(&DelegationStatus::DelegatedTo(s)) if s == *sender => (),
					_ => return Err(Error::InvalidMessage),
				}
			}

			let consensus_group: BTreeSet<NodeId> = message.nodes.keys().cloned().map(Into::into).collect();
			let mut other_consensus_group_nodes = consensus_group.clone();
			other_consensus_group_nodes.remove(&self.core.meta.self_node_id);

			let access_key = self.core.access_key.clone();
			let nonce = self.core.nonce;
			let generation_session = GenerationSession::new(GenerationSessionParams {
				id: self.core.meta.id.clone(),
				self_node_id: self.core.meta.self_node_id.clone(),
				key_storage: None,
				cluster: Arc::new(NonceGenerationTransport {
					access_key: self.core.access_key.clone(),
					cluster: self.core.cluster.clone(),
					nonce: self.core.nonce,
					other_nodes_ids: other_consensus_group_nodes,
					map: move |m| EcdsaSigningMessage::EcdsaInversionZeroGenerationMessage(EcdsaInversionZeroGenerationMessage {
						session: m.session_id().clone().into(),
						sub_session: access_key.clone().into(),
						session_nonce: nonce,
						message: m,
					}),
				}),
				nonce: None,
			});
			data.inv_zero_generation_session = Some(generation_session);
			data.state = SessionState::NoncesGenerating;
		}

		{
			let generation_session = data.inv_zero_generation_session.as_ref().ok_or(Error::InvalidStateForRequest)?;
			let is_key_generating = generation_session.state() != GenerationSessionState::Finished;
			generation_session.process_message(sender, &message.message)?;

			let is_key_generated = generation_session.state() == GenerationSessionState::Finished;
			if !is_key_generating || !is_key_generated {
				return Ok(());
			}
		}

		if !Self::check_nonces_generated(&*data) {
			return Ok(());
		}

		Self::send_inversed_nonce_coeff_share(&self.core, &mut*data)?;
		data.state = if self.core.meta.master_node_id != self.core.meta.self_node_id {
			SessionState::SignatureComputing
		} else {
			SessionState::WaitingForInversedNonceShares
		};

		Ok(())
	}

	/// When inversed nonce share is received.
	pub fn on_inversed_nonce_coeff_share(&self, sender: &NodeId, message: &EcdsaSigningInversedNonceCoeffShare) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		let key_share = match self.core.key_share.as_ref() {
			None => return Err(Error::InvalidMessage),
			Some(key_share) => key_share,
		};

		let mut data = self.data.lock();

		let key_version = key_share.version(data.version.as_ref().expect("TODO")).map_err(|e| Error::KeyStorage(e.into()))?;

		if sender != &self.core.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}
		if data.state != SessionState::WaitingForInversedNonceShares {
			return Err(Error::InvalidStateForRequest);
		}

		let inversed_nonce_coeff = {
			let consensus_group = data.consensus_session.select_consensus_group()?.clone();
			let inversed_nonce_coeff_shares = data.inversed_nonce_coeff_shares.as_mut().expect("TODO");
			match inversed_nonce_coeff_shares.entry(sender.clone()) {
				Entry::Occupied(_) => return Err(Error::InvalidStateForRequest),
				Entry::Vacant(entry) => {
					entry.insert(message.inversed_nonce_coeff_share.clone().into());
				},
			}

			if consensus_group.iter().any(|n| !inversed_nonce_coeff_shares.contains_key(n)) {
				return Ok(());
			}

			let inversed_nonce_coeff = math::compute_inversed_secret_coeff_from_shares(key_share.threshold,
				&inversed_nonce_coeff_shares.keys().map(|n| key_version.id_numbers[n].clone()).collect::<Vec<_>>(),
				&inversed_nonce_coeff_shares.values().cloned().collect::<Vec<_>>())?;

			inversed_nonce_coeff
		};


		let version = data.version.as_ref().ok_or(Error::InvalidMessage)?.clone();
		let message_hash = data.message_hash
			.expect("we are on master node; on master node message_hash is filled in initialize(); on_generation_message follows initialize; qed");

		let nonce_exists_proof = "nonce is generated before signature is computed; we are in SignatureComputing state; qed";
		let sig_nonce_public = data.sig_nonce_generation_session.as_ref().expect(nonce_exists_proof).joint_public_and_secret().expect(nonce_exists_proof)?.0;
		let inv_nonce_share = data.inv_nonce_generation_session.as_ref().expect(nonce_exists_proof).joint_public_and_secret().expect(nonce_exists_proof)?.2;

		self.core.disseminate_jobs(&mut data.consensus_session, &version, sig_nonce_public, inv_nonce_share, inversed_nonce_coeff, message_hash)
	}

	/// When partial signature is requested.
	pub fn on_partial_signature_requested(&self, sender: &NodeId, message: &EcdsaRequestPartialSignature) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		let key_share = match self.core.key_share.as_ref() {
			None => return Err(Error::InvalidMessage),
			Some(key_share) => key_share,
		};

		let mut data = self.data.lock();

		if sender != &self.core.meta.master_node_id {
			return Err(Error::InvalidMessage);
		}
		if data.state != SessionState::SignatureComputing {
			return Err(Error::InvalidStateForRequest);
		}

		let nonce_exists_proof = "nonce is generated before signature is computed; we are in SignatureComputing state; qed";
		let sig_nonce_public = data.sig_nonce_generation_session.as_ref().expect(nonce_exists_proof).joint_public_and_secret().expect(nonce_exists_proof)?.0;
		let inv_nonce_share = data.inv_nonce_generation_session.as_ref().expect(nonce_exists_proof).joint_public_and_secret().expect(nonce_exists_proof)?.2;

		let version = data.version.as_ref().ok_or(Error::InvalidMessage)?.clone();
		let key_version = key_share.version(&version).map_err(|e| Error::KeyStorage(e.into()))?.hash.clone();

		let signing_job = EcdsaSigningJob::new_on_slave(key_share.clone(), key_version, sig_nonce_public, inv_nonce_share)?;
		let signing_transport = self.core.signing_transport();

		data.consensus_session.on_job_request(sender, EcdsaPartialSigningRequest {
			id: message.request_id.clone().into(),
			inversed_nonce_coeff: message.inversed_nonce_coeff.clone().into(),
			message_hash: message.message_hash.clone().into(),
		}, signing_job, signing_transport)
	}

	/// When partial signature is received.
	pub fn on_partial_signature(&self, sender: &NodeId, message: &EcdsaPartialSignature) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		let mut data = self.data.lock();
		data.consensus_session.on_job_response(sender, EcdsaPartialSigningResponse {
			request_id: message.request_id.clone().into(),
			partial_signature_s: message.partial_signature_s.clone().into(),
		})?;

		if data.consensus_session.state() != ConsensusSessionState::Finished {
			return Ok(());
		}

		// send compeltion signal to all nodes, except for rejected nodes
		for node in data.consensus_session.consensus_non_rejected_nodes() {
			self.core.cluster.send(&node, Message::EcdsaSigning(EcdsaSigningMessage::EcdsaSigningSessionCompleted(EcdsaSigningSessionCompleted {
				session: self.core.meta.id.clone().into(),
				sub_session: self.core.access_key.clone().into(),
				session_nonce: self.core.nonce,
			})))?;
		}

		let result = data.consensus_session.result()?;
		Self::set_signing_result(&self.core, &mut *data, Ok(result));

		Ok(())
	}

	/// When session is completed.
	pub fn on_session_completed(&self, sender: &NodeId, message: &EcdsaSigningSessionCompleted) -> Result<(), Error> {
		debug_assert!(self.core.meta.id == *message.session);
		debug_assert!(self.core.access_key == *message.sub_session);
		debug_assert!(sender != &self.core.meta.self_node_id);

		self.data.lock().consensus_session.on_session_completed(sender)
	}

	/// Process error from the other node.
	fn process_node_error(&self, node: Option<&NodeId>, error: Error) -> Result<(), Error> {
		let mut data = self.data.lock();
		let is_self_node_error = node.map(|n| n == &self.core.meta.self_node_id).unwrap_or(false);
		// error is always fatal if coming from this node
		if is_self_node_error {
			Self::set_signing_result(&self.core, &mut *data, Err(error.clone()));
			return Err(error);
		}

		match {
			match node {
				Some(node) => data.consensus_session.on_node_error(node),
				None => data.consensus_session.on_session_timeout(),
			}
		} {
			Ok(false) => {
				Ok(())
			},
			Ok(true) => {
				/*let version = data.version.as_ref().ok_or(Error::InvalidMessage)?.clone();
				let message_hash = data.message_hash.as_ref().cloned()
					.expect("on_node_error returned true; this means that jobs must be REsent; this means that jobs already have been sent; jobs are sent when message_hash.is_some(); qed");
				let joint_public_and_secret = data.generation_session.as_ref()
					.expect("on_node_error returned true; this means that jobs must be REsent; this means that jobs already have been sent; jobs are sent when message_hash.is_some(); qed")
					.joint_public_and_secret()
					.expect("on_node_error returned true; this means that jobs must be REsent; this means that jobs already have been sent; jobs are sent when message_hash.is_some(); qed")?;
				let disseminate_result = self.core.disseminate_jobs(&mut data.consensus_session, &version, joint_public_and_secret.0, joint_public_and_secret.1, message_hash);
				match disseminate_result {
					Ok(()) => Ok(()),
					Err(err) => {
						warn!("{}: signing session failed with error: {:?} from {:?}", &self.core.meta.self_node_id, error, node);
						Self::set_signing_result(&self.core, &mut *data, Err(err.clone()));
						Err(err)
					}
				}*/
				unimplemented!("TODO")
			},
			Err(err) => {
				warn!("{}: signing session failed with error: {:?} from {:?}", &self.core.meta.self_node_id, error, node);
				Self::set_signing_result(&self.core, &mut *data, Err(err.clone()));
				Err(err)
			},
		}
	}

	/// Set signing session result.
	fn set_signing_result(core: &SessionCore, data: &mut SessionData, result: Result<Signature, Error>) {
		if let Some(DelegationStatus::DelegatedFrom(master, nonce)) = data.delegation_status.take() {
			// error means can't communicate => ignore it
			let _ = match result.as_ref() {
				Ok(signature) => core.cluster.send(&master, Message::EcdsaSigning(EcdsaSigningMessage::EcdsaSigningSessionDelegationCompleted(EcdsaSigningSessionDelegationCompleted {
					session: core.meta.id.clone().into(),
					sub_session: core.access_key.clone().into(),
					session_nonce: nonce,
					signature: signature.clone().into(),
				}))),
				Err(error) => core.cluster.send(&master, Message::EcdsaSigning(EcdsaSigningMessage::EcdsaSigningSessionError(EcdsaSigningSessionError {
					session: core.meta.id.clone().into(),
					sub_session: core.access_key.clone().into(),
					session_nonce: nonce,
					error: error.clone().into(),
				}))),
			};
		}

		data.result = Some(result);
		core.completed.notify_all();
	}

	/// Check if all nonces are generated.
	fn check_nonces_generated(data: &SessionData) -> bool {
		let expect_proof = "check_nonces_generated is called when som nonce-gen session is completed;
			all nonce-gen sessions are created at once; qed";
		let sig_nonce_generation_session = data.sig_nonce_generation_session.as_ref().expect(expect_proof);
		let inv_nonce_generation_session = data.inv_nonce_generation_session.as_ref().expect(expect_proof);
		let inv_zero_generation_session = data.inv_zero_generation_session.as_ref().expect(expect_proof);
		let are_generated = sig_nonce_generation_session.state() == GenerationSessionState::Finished
			&& inv_nonce_generation_session.state() == GenerationSessionState::Finished
			&& inv_zero_generation_session.state() == GenerationSessionState::Finished;
if are_generated {
	println!("=== {:?}: nonce_share={:?}", sig_nonce_generation_session.node(), *sig_nonce_generation_session.joint_public_and_secret().unwrap().unwrap().2);
	println!("=== {:?}: inv_nonce_share={:?}", sig_nonce_generation_session.node(), *inv_nonce_generation_session.joint_public_and_secret().unwrap().unwrap().2);
	println!("=== {:?}: inv_zero_share={:?}", sig_nonce_generation_session.node(), *inv_zero_generation_session.joint_public_and_secret().unwrap().unwrap().2);
}
		are_generated
	}

	/// Broadcast inversed nonce share.
	fn send_inversed_nonce_coeff_share(core: &SessionCore, data: &mut SessionData) -> Result<(), Error> {
		let key_share = match core.key_share.as_ref() {
			None => return Err(Error::InvalidMessage),
			Some(key_share) => key_share,
		};
		let key_version = key_share.version(data.version.as_ref().expect("TODO")).map_err(|e| Error::KeyStorage(e.into()))?;

		let inv_nonce_generation_session = data.inv_nonce_generation_session.as_ref().expect("TODO");
		let inv_nonce = inv_nonce_generation_session.joint_public_and_secret().expect("TODO").expect("TODO").2;

		let inv_zero_generation_session = data.inv_zero_generation_session.as_ref().expect("TODO");
		let inv_zero = inv_nonce_generation_session.joint_public_and_secret().expect("TODO").expect("TODO").2;

		let inversed_nonce_coeff_share = math::compute_ecdsa_inversed_secret_coeff_share(&key_version.secret_share, &inv_nonce, &inv_zero)?;
		core.cluster.send(&core.meta.master_node_id, Message::EcdsaSigning(EcdsaSigningMessage::EcdsaSigningInversedNonceCoeffShare(EcdsaSigningInversedNonceCoeffShare {
			session: core.meta.id.clone().into(),
			sub_session: core.access_key.clone().into(),
			session_nonce: core.nonce,
			inversed_nonce_coeff_share: inversed_nonce_coeff_share.into(),
		})))
/*		let version = data.version.as_ref().ok_or(Error::InvalidMessage)?.clone();
		let message_hash = data.message_hash
			.expect("we are on master node; on master node message_hash is filled in initialize(); on_generation_message follows initialize; qed");

		let nonce_exists_proof = "nonce is generated before signature is computed; we are in SignatureComputing state; qed";
		let sig_nonce_public = data.sig_nonce_generation_session.as_ref().expect(nonce_exists_proof).joint_public_and_secret().expect(nonce_exists_proof)?.0;
		let inv_nonce_share = data.inv_nonce_generation_session.as_ref().expect(nonce_exists_proof).joint_public_and_secret().expect(nonce_exists_proof)?.2;

		core.disseminate_jobs(&mut data.consensus_session, &version, sig_nonce_public, inv_nonce_share, message_hash)*/
	}
}

impl ClusterSession for SessionImpl {
	type Id = SessionIdWithSubSession;

	fn type_name() -> &'static str {
		"ecdsa_signing"
	}

	fn id(&self) -> SessionIdWithSubSession {
		SessionIdWithSubSession::new(self.core.meta.id.clone(), self.core.access_key.clone())
	}

	fn is_finished(&self) -> bool {
		let data = self.data.lock();
		data.consensus_session.state() == ConsensusSessionState::Failed
			|| data.consensus_session.state() == ConsensusSessionState::Finished
			|| data.result.is_some()
	}

	fn on_node_timeout(&self, node: &NodeId) {
		// ignore error, only state matters
		let _ = self.process_node_error(Some(node), Error::NodeDisconnected);
	}

	fn on_session_timeout(&self) {
		// ignore error, only state matters
		let _ = self.process_node_error(None, Error::NodeDisconnected);
	}

	fn on_session_error(&self, node: &NodeId, error: Error) {
		let is_fatal = self.process_node_error(Some(node), error.clone()).is_err();
		let is_this_node_error = *node == self.core.meta.self_node_id;
		if is_fatal || is_this_node_error {
			// error in signing session is non-fatal, if occurs on slave node
			// => either respond with error
			// => or broadcast error
			let message = Message::EcdsaSigning(EcdsaSigningMessage::EcdsaSigningSessionError(EcdsaSigningSessionError {
				session: self.core.meta.id.clone().into(),
				sub_session: self.core.access_key.clone().into(),
				session_nonce: self.core.nonce,
				error: error.clone().into(),
			}));

			// do not bother processing send error, as we already processing error
			let _ = if self.core.meta.master_node_id == self.core.meta.self_node_id {
				self.core.cluster.broadcast(message)
			} else {
				self.core.cluster.send(&self.core.meta.master_node_id, message)
			};
		}
	}

	fn on_message(&self, sender: &NodeId, message: &Message) -> Result<(), Error> {
		match *message {
			Message::EcdsaSigning(ref message) => self.process_message(sender, message),
			_ => unreachable!("cluster checks message to be correct before passing; qed"),
		}
	}
}

impl<F> NonceGenerationTransport<F> where F: Fn(GenerationMessage) -> EcdsaSigningMessage + Send + Sync {
	fn map_message(&self, message: Message) -> Result<Message, Error> {
		match message {
			Message::Generation(message) => Ok(Message::EcdsaSigning((self.map)(message))),
			_ => Err(Error::InvalidMessage),
		}
	}
}

impl<F> Cluster for NonceGenerationTransport<F> where F: Fn(GenerationMessage) -> EcdsaSigningMessage + Send + Sync {
	fn broadcast(&self, message: Message) -> Result<(), Error> {
		let message = self.map_message(message)?;
		for to in &self.other_nodes_ids {
			self.cluster.send(to, message.clone())?;
		}
		Ok(())
	}

	fn send(&self, to: &NodeId, message: Message) -> Result<(), Error> {
		debug_assert!(self.other_nodes_ids.contains(to));
		self.cluster.send(to, self.map_message(message)?)
	}

	fn is_connected(&self, node: &NodeId) -> bool {
		self.cluster.is_connected(node)
	}

	fn nodes(&self) -> BTreeSet<NodeId> {
		self.cluster.nodes()
	}
}

impl SessionCore {
	pub fn signing_transport(&self) -> SigningJobTransport {
		SigningJobTransport {
			id: self.meta.id.clone(),
			access_key: self.access_key.clone(),
			nonce: self.nonce,
			cluster: self.cluster.clone()
		}
	}

	pub fn disseminate_jobs(&self, consensus_session: &mut SigningConsensusSession, version: &H256, nonce_public: Public, inv_nonce_share: Secret, inversed_nonce_coeff: Secret, message_hash: H256) -> Result<(), Error> {
		let key_share = match self.key_share.as_ref() {
			None => return Err(Error::InvalidMessage),
			Some(key_share) => key_share,
		};

		let key_version = key_share.version(version).map_err(|e| Error::KeyStorage(e.into()))?.hash.clone();
		let signing_job = EcdsaSigningJob::new_on_master(key_share.clone(), key_version, nonce_public, inv_nonce_share, inversed_nonce_coeff, message_hash)?;
		consensus_session.disseminate_jobs(signing_job, self.signing_transport())
	}
}

impl JobTransport for SigningConsensusTransport {
	type PartialJobRequest=Signature;
	type PartialJobResponse=bool;

	fn send_partial_request(&self, node: &NodeId, request: Signature) -> Result<(), Error> {
		let version = self.version.as_ref()
			.expect("send_partial_request is called on initialized master node only; version is filled in before initialization starts on master node; qed");
		self.cluster.send(node, Message::EcdsaSigning(EcdsaSigningMessage::EcdsaSigningConsensusMessage(EcdsaSigningConsensusMessage {
			session: self.id.clone().into(),
			sub_session: self.access_key.clone().into(),
			session_nonce: self.nonce,
			message: ConsensusMessage::InitializeConsensusSession(InitializeConsensusSession {
				requestor_signature: request.into(),
				version: version.clone().into(),
			})
		})))
	}

	fn send_partial_response(&self, node: &NodeId, response: bool) -> Result<(), Error> {
		self.cluster.send(node, Message::EcdsaSigning(EcdsaSigningMessage::EcdsaSigningConsensusMessage(EcdsaSigningConsensusMessage {
			session: self.id.clone().into(),
			sub_session: self.access_key.clone().into(),
			session_nonce: self.nonce,
			message: ConsensusMessage::ConfirmConsensusInitialization(ConfirmConsensusInitialization {
				is_confirmed: response,
			})
		})))
	}
}

impl JobTransport for SigningJobTransport {
	type PartialJobRequest=EcdsaPartialSigningRequest;
	type PartialJobResponse=EcdsaPartialSigningResponse;

	fn send_partial_request(&self, node: &NodeId, request: EcdsaPartialSigningRequest) -> Result<(), Error> {
		self.cluster.send(node, Message::EcdsaSigning(EcdsaSigningMessage::EcdsaRequestPartialSignature(EcdsaRequestPartialSignature {
			session: self.id.clone().into(),
			sub_session: self.access_key.clone().into(),
			session_nonce: self.nonce,
			request_id: request.id.into(),
			inversed_nonce_coeff: request.inversed_nonce_coeff.into(),
			message_hash: request.message_hash.into(),
		})))
	}

	fn send_partial_response(&self, node: &NodeId, response: EcdsaPartialSigningResponse) -> Result<(), Error> {
		self.cluster.send(node, Message::EcdsaSigning(EcdsaSigningMessage::EcdsaPartialSignature(EcdsaPartialSignature {
			session: self.id.clone().into(),
			sub_session: self.access_key.clone().into(),
			session_nonce: self.nonce,
			request_id: response.request_id.into(),
			partial_signature_s: response.partial_signature_s.into(),
		})))
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;
	use std::str::FromStr;
	use std::collections::{BTreeMap, VecDeque};
	use ethereum_types::H256;
	use ethkey::{self, Random, Generator, Public, Secret, KeyPair, verify_public};
	use acl_storage::DummyAclStorage;
	use key_server_cluster::{NodeId, DummyKeyStorage, DocumentKeyShare, DocumentKeyShareVersion, SessionId, SessionMeta, Error, KeyStorage};
	use key_server_cluster::cluster_sessions::ClusterSession;
	use key_server_cluster::cluster::tests::DummyCluster;
	use key_server_cluster::generation_session::tests::MessageLoop as KeyGenerationMessageLoop;
	use key_server_cluster::math;
	use key_server_cluster::message::{Message, SchnorrSigningMessage, SchnorrSigningConsensusMessage, ConsensusMessage, ConfirmConsensusInitialization,
		SchnorrSigningGenerationMessage, GenerationMessage, ConfirmInitialization, InitializeSession, SchnorrRequestPartialSignature};
	use key_server_cluster::signing_session_ecdsa::{SessionImpl, SessionState, SessionParams};

	struct Node {
		pub node_id: NodeId,
		pub cluster: Arc<DummyCluster>,
		pub key_storage: Arc<DummyKeyStorage>,
		pub session: SessionImpl,
	}

	struct MessageLoop {
		pub session_id: SessionId,
		pub requester: KeyPair,
		pub nodes: BTreeMap<NodeId, Node>,
		pub queue: VecDeque<(NodeId, NodeId, Message)>,
		pub acl_storages: Vec<Arc<DummyAclStorage>>,
		pub version: H256,
	}

	impl MessageLoop {
		pub fn new(gl: &KeyGenerationMessageLoop) -> Self {
			let version = gl.nodes.values().nth(0).unwrap().key_storage.get(&Default::default()).unwrap().unwrap().versions.iter().last().unwrap().hash;
			let mut nodes = BTreeMap::new();
			let session_id = gl.session_id.clone();
			let requester = Random.generate().unwrap();
			let signature = Some(ethkey::sign(requester.secret(), &SessionId::default()).unwrap());
			let master_node_id = gl.nodes.keys().nth(0).unwrap().clone();
			let mut acl_storages = Vec::new();
			for (i, (gl_node_id, gl_node)) in gl.nodes.iter().enumerate() {
				let acl_storage = Arc::new(DummyAclStorage::default());
				acl_storages.push(acl_storage.clone());
				let cluster = Arc::new(DummyCluster::new(gl_node_id.clone()));
				let session = SessionImpl::new(SessionParams {
					meta: SessionMeta {
						id: session_id.clone(),
						self_node_id: gl_node_id.clone(),
						master_node_id: master_node_id.clone(),
						threshold: gl_node.key_storage.get(&session_id).unwrap().unwrap().threshold,
					},
					access_key: "834cb736f02d9c968dfaf0c37658a1d86ff140554fc8b59c9fdad5a8cf810eec".parse().unwrap(),
					key_share: Some(gl_node.key_storage.get(&session_id).unwrap().unwrap()),
					acl_storage: acl_storage,
					cluster: cluster.clone(),
					nonce: 0,
				}, if i == 0 { signature.clone() } else { None }).unwrap();
				nodes.insert(gl_node_id.clone(), Node { node_id: gl_node_id.clone(), cluster: cluster, key_storage: gl_node.key_storage.clone(), session: session });
			}

			let nodes_ids: Vec<_> = nodes.keys().cloned().collect();
			for node in nodes.values() {
				for node_id in &nodes_ids {
					node.cluster.add_node(node_id.clone());
				}
			}

			MessageLoop {
				session_id: session_id,
				requester: requester,
				nodes: nodes,
				queue: VecDeque::new(),
				acl_storages: acl_storages,
				version: version,
			}
		}

		pub fn master(&self) -> &SessionImpl {
			&self.nodes.values().nth(0).unwrap().session
		}

		pub fn take_message(&mut self) -> Option<(NodeId, NodeId, Message)> {
			self.nodes.values()
				.filter_map(|n| n.cluster.take_message().map(|m| (n.node_id.clone(), m.0, m.1)))
				.nth(0)
				.or_else(|| self.queue.pop_front())
		}

		pub fn process_message(&mut self, mut msg: (NodeId, NodeId, Message)) -> Result<(), Error> {
println!("=== {:?} -> {:?}: {}", msg.0, msg.1, msg.2);
			let mut is_queued_message = false;
			loop {
				match self.nodes[&msg.1].session.on_message(&msg.0, &msg.2) {
					Ok(_) => {
						if let Some(message) = self.queue.pop_front() {
							msg = message;
							is_queued_message = true;
							continue;
						}
						return Ok(());
					},
					Err(Error::TooEarlyForRequest) => {
						if is_queued_message {
							self.queue.push_front(msg);
						} else {
							self.queue.push_back(msg);
						}
						return Ok(());
					},
					Err(err) => return Err(err),
				}
			}
		}

		pub fn run_until<F: Fn(&MessageLoop) -> bool>(&mut self, predicate: F) -> Result<(), Error> {
			while let Some((from, to, message)) = self.take_message() {
				if predicate(self) {
					return Ok(());
				}

				self.process_message((from, to, message))?;
			}

			unreachable!("either wrong predicate, or failing test")
		}
	}

	fn prepare_signing_sessions(threshold: usize, num_nodes: usize) -> (KeyGenerationMessageLoop, MessageLoop) {
		// run key generation sessions
		let mut gl = KeyGenerationMessageLoop::new(num_nodes);
		gl.master().initialize(Public::default(), false, threshold, gl.nodes.keys().cloned().collect()).unwrap();
		while let Some((from, to, message)) = gl.take_message() {
			gl.process_message((from, to, message)).unwrap();
		}

		// run signing session
		let sl = MessageLoop::new(&gl);
		(gl, sl)
	}

	#[test]
	fn complete_gen_ecdsa_sign_session() {
		let test_cases = [(2, 5)];
		//let test_cases = [(0, 1), (0, 5), (2, 5), (3, 5)];
		for &(threshold, num_nodes) in &test_cases {
			let (gl, mut sl) = prepare_signing_sessions(threshold, num_nodes);
			let key_pair = gl.compute_key_pair(threshold);

			// run signing session
			let message_hash = H256::from(777);
			sl.master().initialize(sl.version.clone(), message_hash).unwrap();
			while let Some((from, to, message)) = sl.take_message() {
				sl.process_message((from, to, message)).unwrap();
			}

			// verify signature
			let public = gl.master().joint_public_and_secret().unwrap().unwrap().0;
			let signature = sl.master().wait().unwrap();
			assert!(verify_public(key_pair.public(), &signature, &message_hash).unwrap());
		}
	}
}

/*

=== M -> a: EcdsaSigning.EcdsaSigningConsensusMessage.InitializeConsensusSession
=== M -> b: EcdsaSigning.EcdsaSigningConsensusMessage.InitializeConsensusSession
=== M -> c: EcdsaSigning.EcdsaSigningConsensusMessage.InitializeConsensusSession
=== M -> d: EcdsaSigning.EcdsaSigningConsensusMessage.InitializeConsensusSession
=== a -> M: EcdsaSigning.EcdsaSigningConsensusMessage.ConfirmConsensusInitialization(true)
=== b -> M: EcdsaSigning.EcdsaSigningConsensusMessage.ConfirmConsensusInitialization(true)
=== c -> M: EcdsaSigning.EcdsaSigningConsensusMessage.ConfirmConsensusInitialization(true)
=== d -> M: EcdsaSigning.EcdsaSigningConsensusMessage.ConfirmConsensusInitialization(true)
=== M -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.InitializeSession
=== M -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.InitializeSession
=== M -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.InitializeSession
=== a -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.ConfirmInitialization
=== M -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.InitializeSession
=== a -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.ConfirmInitialization
=== M -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.InitializeSession
=== a -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.ConfirmInitialization
=== M -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.InitializeSession
=== b -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.ConfirmInitialization
=== M -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.InitializeSession
=== b -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.ConfirmInitialization
=== M -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.InitializeSession
=== b -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.ConfirmInitialization
=== M -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.InitializeSession
=== c -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.ConfirmInitialization
=== M -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.InitializeSession
=== c -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.ConfirmInitialization
=== M -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.InitializeSession
=== c -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.ConfirmInitialization
=== M -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.InitializeSession
=== d -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.ConfirmInitialization
=== M -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.CompleteInitialization
=== M -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.CompleteInitialization
=== M -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.CompleteInitialization
=== M -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.CompleteInitialization
=== M -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== M -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== M -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== M -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== a -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== a -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== a -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== a -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== b -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== b -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== b -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== b -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== c -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== c -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== c -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== c -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== d -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.ConfirmInitialization
=== M -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.CompleteInitialization
=== M -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.CompleteInitialization
=== M -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.CompleteInitialization
=== M -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.CompleteInitialization
=== M -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== M -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== M -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== M -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== a -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== a -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== a -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== a -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== b -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== b -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== b -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== b -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== c -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== c -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== c -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== c -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== d -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.ConfirmInitialization
=== M -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.CompleteInitialization
=== M -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.CompleteInitialization
=== M -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.CompleteInitialization
=== M -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.CompleteInitialization
=== M -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== M -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== M -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== M -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== a -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== a -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== a -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== a -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== b -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== b -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== b -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== b -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== c -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== c -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== c -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== c -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== d -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== M -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== M -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== M -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== M -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== d -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== a -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== a -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== a -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== a -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== d -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== b -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== b -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== b -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== b -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== d -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.KeysDissemination
=== c -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== c -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== c -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== c -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== d -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== M -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.SessionCompleted
=== M -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.SessionCompleted
=== M -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.SessionCompleted
=== M -> d: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.SessionCompleted
=== d -> a: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== a -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.SessionCompleted
=== d -> b: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== b -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.SessionCompleted
=== d -> c: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.PublicKeyShare
=== c -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.SessionCompleted
=== d -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== M -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== M -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== M -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== M -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== d -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== a -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== a -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== a -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== a -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== d -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== b -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== b -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== b -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== b -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== d -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.KeysDissemination
=== c -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== c -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== c -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== c -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== d -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== M -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.SessionCompleted
=== M -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.SessionCompleted
=== M -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.SessionCompleted
=== M -> d: EcdsaSigning.EcdsaInversionNonceGenerationMessage.SessionCompleted
=== d -> a: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== a -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.SessionCompleted
=== d -> b: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== b -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.SessionCompleted
=== d -> c: EcdsaSigning.EcdsaInversionNonceGenerationMessage.PublicKeyShare
=== c -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.SessionCompleted
=== d -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== M -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== M -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== M -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== M -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== d -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== a -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== a -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== a -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== a -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== d -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== b -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== b -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== b -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== b -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== d -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.KeysDissemination
=== c -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== c -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== c -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== c -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== d -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== M -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.SessionCompleted
=== M -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.SessionCompleted
=== M -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.SessionCompleted
=== M -> d: EcdsaSigning.EcdsaInversionZeroGenerationMessage.SessionCompleted
=== d: nonce_share=cfe6a8c05848b0dd931b3fb08ea1d1de64cf3dbeda42bf3944ee42ec82506b25
=== d: inv_nonce_share=143f9bb227cb826595295e87c69793ac6b46ab5c48b6f13820459bd1961e8436
=== d: inv_zero_share=071d9627b80e55a8285b29e7e781c73a3ee71952d7d7a1b48049648ff6e4efc3
=== d -> a: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== a: nonce_share=c4bd58b2f4589b7ef685592c44e55257782032aed3bba66e0173cab778702c5e
=== a: inv_nonce_share=6ae4daa7bb16154fef08cba38251935b76f9f7c2b9c627a8ce849245bce784cc
=== a: inv_zero_share=0e506badd592f638322ff4eb9da2ac1964b647012ef6b586074e32e2825d0e56
=== a -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.SessionCompleted
=== d -> b: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== b: nonce_share=0e394007b322a6bab7006551bfa5f99bea783749e852cff44b701cf6637d27f4
=== b: inv_nonce_share=141f80f0e26861676b622959a52fd853baa5251526f22527b9b36c6b422a2023
=== b: inv_zero_share=0f50bfdcc207985dc29f2907de543b5e25917ba40b5834e9fbcbe85f8b8f4644
=== b -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.SessionCompleted
=== d -> c: EcdsaSigning.EcdsaInversionZeroGenerationMessage.PublicKeyShare
=== c: nonce_share=703fa05efafc61dd2a9378afa08d98864eb1cb8084ef16764ba4e89480d80686
=== c: inv_nonce_share=39ebe7afc6dffced32b9a9744dc4913ceed0d485b57e2d577516504e67da8dbb
=== c: inv_zero_share=c040a5f3575d5987d6f8541255d26f66cb77f8a4f445cebfe4a947463154cc39
=== c -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.SessionCompleted
=== d -> M: EcdsaSigning.EcdsaSignatureNonceGenerationMessage.SessionCompleted
=== d -> M: EcdsaSigning.EcdsaInversionNonceGenerationMessage.SessionCompleted
=== d -> M: EcdsaSigning.EcdsaInversionZeroGenerationMessage.SessionCompleted
=== M: nonce_share=339434da07db1079f4f730ba79cf972f0b4c7dd3edb32ffcb40bc813035bb656
=== M: inv_nonce_share=a14e441f6cd71ace97b51bd2637874b28bff9a63a0a6f4f7bb5dab8bde3abcb5
=== M: inv_zero_share=2b3626b9588b1cce0d8a8e1ca41114e36996bdba441dd368cf82bd83f4fc7163
=== partial_signature_s(nonce_public=05851932b28617e16ecf87afcbe502a8ac47e40a7a6aba11231364e2ec618fa2d3421b4681edbee4f0dc01f1001a2cb0048830ff52a2796994e0d4b74f63a99d, inv_nonce_share=a14e441f6cd71ace97b51bd2637874b28bff9a63a0a6f4f7bb5dab8bde3abcb5, secret_share=586a58a5f60e7b4b97d95ba4449f45dfe0b158db64d5bb5ebcf917104f27d23e) = Secret: 0x64ae..9d6e
=== M -> a: EcdsaSigning.EcdsaRequestPartialSignature
=== partial_signature_s(nonce_public=05851932b28617e16ecf87afcbe502a8ac47e40a7a6aba11231364e2ec618fa2d3421b4681edbee4f0dc01f1001a2cb0048830ff52a2796994e0d4b74f63a99d, inv_nonce_share=6ae4daa7bb16154fef08cba38251935b76f9f7c2b9c627a8ce849245bce784cc, secret_share=bca9a64c446b6c9adb93fb5ce40bbceb28d45fcbc04cc5546a6101d480ff82dc) = Secret: 0xea82..b26e
=== M -> b: EcdsaSigning.EcdsaRequestPartialSignature
=== partial_signature_s(nonce_public=05851932b28617e16ecf87afcbe502a8ac47e40a7a6aba11231364e2ec618fa2d3421b4681edbee4f0dc01f1001a2cb0048830ff52a2796994e0d4b74f63a99d, inv_nonce_share=141f80f0e26861676b622959a52fd853baa5251526f22527b9b36c6b422a2023, secret_share=fbfb1ff207ccd3f339c4d8ab3b64c1f4dee941660a8823b6279c1bb66f925613) = Secret: 0x717b..3a8
=== M -> c: EcdsaSigning.EcdsaRequestPartialSignature
=== partial_signature_s(nonce_public=05851932b28617e16ecf87afcbe502a8ac47e40a7a6aba11231364e2ec618fa2d3421b4681edbee4f0dc01f1001a2cb0048830ff52a2796994e0d4b74f63a99d, inv_nonce_share=39ebe7afc6dffced32b9a9744dc4913ceed0d485b57e2d577516504e67da8dbb, secret_share=bf79c369f6cb21df02f7d36dd45253c16b666e29c3af0278481a9bb48cd75a0b) = Secret: 0x62b8..3ce
=== M -> d: EcdsaSigning.EcdsaRequestPartialSignature
=== partial_signature_s(nonce_public=05851932b28617e16ecf87afcbe502a8ac47e40a7a6aba11231364e2ec618fa2d3421b4681edbee4f0dc01f1001a2cb0048830ff52a2796994e0d4b74f63a99d, inv_nonce_share=143f9bb227cb826595295e87c69793ac6b46ab5c48b6f13820459bd1961e8436, secret_share=ed88dbfdce641d7c55cfb38f57abe988b466195f576321288956853f2726024b) = Secret: 0xf941..ef91
=== a -> M: EcdsaSigning.EcdsaPartialSignature
=== b -> M: EcdsaSigning.EcdsaPartialSignature
=== c -> M: EcdsaSigning.EcdsaPartialSignature
=== d -> M: EcdsaSigning.EcdsaPartialSignature
=== M -> a: EcdsaSigning.EcdsaSigningSessionCompleted
=== M -> b: EcdsaSigning.EcdsaSigningSessionCompleted
=== M -> c: EcdsaSigning.EcdsaSigningSessionCompleted
=== M -> d: EcdsaSigning.EcdsaSigningSessionCompleted

*/