use std::collections::HashSet;

use bytes::{Bytes, BytesMut};
use mail_protocol_core::ProtocolError;

use crate::CapabilitySet;

use super::{PendingCommand, client::InFlight, semantics::invalid_state};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EnableResponse {
    pub(super) capabilities: CapabilitySet,
    pub(super) compatible_requests: HashSet<u64>,
}

pub(super) struct EnableCompatibility {
    pub(super) edges: Vec<Vec<bool>>,
    pub(super) request_count: usize,
}

impl EnableCompatibility {
    pub(super) fn has_matching(
        &self,
        excluded_request: Option<usize>,
        excluded_response: Option<usize>,
    ) -> bool {
        let response_count = self
            .edges
            .len()
            .saturating_sub(usize::from(excluded_response.is_some()));
        let request_count = self
            .request_count
            .saturating_sub(usize::from(excluded_request.is_some()));
        if response_count > request_count {
            return false;
        }

        let mut response_to_request: Vec<Option<usize>> = vec![None; self.edges.len()];
        let mut request_to_response: Vec<Option<usize>> = vec![None; self.request_count];
        for root in 0..self.edges.len() {
            if excluded_response == Some(root) {
                continue;
            }
            let mut response_queue = vec![root];
            let mut queue_cursor = 0usize;
            let mut seen_responses = vec![false; self.edges.len()];
            let mut seen_requests = vec![false; self.request_count];
            let mut parent_request = vec![None; self.request_count];
            let mut parent_response = vec![None; self.edges.len()];
            seen_responses[root] = true;
            let mut unmatched_request = None;

            while queue_cursor < response_queue.len() && unmatched_request.is_none() {
                let response_index = response_queue[queue_cursor];
                queue_cursor += 1;
                for request_index in 0..self.request_count {
                    if excluded_request == Some(request_index)
                        || seen_requests[request_index]
                        || !self.edges[response_index][request_index]
                    {
                        continue;
                    }
                    seen_requests[request_index] = true;
                    parent_request[request_index] = Some(response_index);
                    if let Some(matched_response) = request_to_response[request_index] {
                        if !seen_responses[matched_response] {
                            seen_responses[matched_response] = true;
                            parent_response[matched_response] = Some(request_index);
                            response_queue.push(matched_response);
                        }
                    } else {
                        unmatched_request = Some(request_index);
                        break;
                    }
                }
            }

            let Some(mut request_index) = unmatched_request else {
                return false;
            };
            loop {
                let Some(response_index) = parent_request[request_index] else {
                    return false;
                };
                let displaced_request = response_to_request[response_index];
                response_to_request[response_index] = Some(request_index);
                request_to_response[request_index] = Some(response_index);
                if response_index == root {
                    break;
                }
                let Some(previous_request) = parent_response[response_index] else {
                    return false;
                };
                debug_assert_eq!(displaced_request, Some(previous_request));
                request_to_response[previous_request] = None;
                request_index = previous_request;
            }
        }
        true
    }
}

pub(super) fn enable_compatibility_graph<'a>(
    pending: &[InFlight],
    responses: impl IntoIterator<Item = &'a EnableResponse>,
) -> Result<EnableCompatibility, ProtocolError> {
    let mut requests = Vec::new();
    for pending in pending {
        if pending.command != PendingCommand::Enable {
            continue;
        }
        let Some(request_id) = pending.enable_request_id else {
            return Err(invalid_state(
                "IMAP ENABLE command lost requested capabilities",
            ));
        };
        requests.push(request_id);
    }
    let responses = responses.into_iter().collect::<Vec<_>>();
    if responses.len() > requests.len() {
        return Ok(EnableCompatibility {
            edges: vec![vec![false; requests.len()]; responses.len()],
            request_count: requests.len(),
        });
    }

    let mut edges = Vec::with_capacity(responses.len());
    for response in responses {
        edges.push(
            requests
                .iter()
                .map(|request_id| response.compatible_requests.contains(request_id))
                .collect(),
        );
    }
    Ok(EnableCompatibility {
        edges,
        request_count: requests.len(),
    })
}

pub(super) fn capability_keys(
    capabilities: &CapabilitySet,
) -> Result<HashSet<Bytes>, ProtocolError> {
    capabilities
        .iter()
        .map(|capability| {
            let mut key = BytesMut::new();
            capability.encode(&mut key)?;
            key.make_ascii_uppercase();
            Ok(key.freeze())
        })
        .collect()
}
