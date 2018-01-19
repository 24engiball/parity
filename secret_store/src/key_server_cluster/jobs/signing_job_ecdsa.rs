/*// Copyright 2015-2017 Parity Technologies (UK) Ltd.
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
use ethkey::{Public, Secret};
use ethereum_types::H256;
use key_server_cluster::{Error, NodeId, DocumentKeyShare};
use key_server_cluster::math;
use key_server_cluster::jobs::job_session::{JobPartialRequestAction, JobPartialResponseAction, JobExecutor};

/// Signing job.
pub struct SigningJob {
	/// This node id.
	self_node_id: NodeId,
	/// Key share.
	key_share: DocumentKeyShare,
	/// Key version.
	key_version: H256,
	/// Session public key.
	session_public: Public,
	/// Session secret coefficient.
	session_secret_coeff: Secret,
	/// Request id.
	request_id: Option<Secret>,
	/// Message hash.
	message_hash: Option<H256>,
}

/// Signing job partial request.
pub struct PartialSigningRequest {
	/// Request id.
	pub id: Secret,
	/// Message hash.
	pub message_hash: H256,
	/// Id of other nodes, participating in signing.
	pub other_nodes_ids: BTreeSet<NodeId>,
}

/// Signing job partial response.
pub struct PartialSigningResponse {
	/// Request id.
	pub request_id: Secret,
	/// Partial signature.
	pub partial_signature: Secret,
}
*/