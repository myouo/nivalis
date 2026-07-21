//! Runtime-independent IMAP framing and common command/response types.

#![forbid(unsafe_code)]

mod append;
mod astring;
mod capability;
mod codec;
mod command;
mod command_frame;
mod esearch;
mod fetch;
mod fetch_response;
mod id;
mod list;
mod namespace;
mod quota;
mod response;
mod search;
mod search_response;
mod select;
mod session;
mod sort;
mod status_items;
mod syntax;
mod tagged_ext;
mod thread;
mod types;

pub use append::{AppendArguments, AppendFlags};
pub use astring::{AString, AStringKind};
pub use capability::{Capability, CapabilitySet};
pub use codec::{
    AuthenticateContinuationDecoder, AuthenticateContinuationEncoder, ClientCommandTransmission,
    CommandDecoder, CommandEncoder, CommandSendStep, IdleDoneDecoder, IdleDoneEncoder,
    LiteralRequest, ResponseDecoder, ResponseEncoder, ServerCommandDecoder, ServerCommandStatus,
};
pub use command_frame::CommandFrame;
pub use esearch::{DEFAULT_ESEARCH_MAX_DEPTH, ESearchItem, ESearchItemIter, ESearchResponse};
pub use fetch::{
    DEFAULT_FETCH_MAX_DEPTH, FetchArguments, FetchAttribute, FetchAttributeIter,
    FetchHeaderFieldIter, FetchHeaderFields, FetchMacro, FetchModifier, FetchModifierIter,
    FetchPartial, FetchSection, FetchSectionPartIter, FetchSectionText,
};
pub use fetch_response::{
    BodyDisposition, BodyExtensionIter, BodyExtensions, BodyFields, BodyLanguage, BodyLanguageIter,
    BodyParameter, BodyParameterIter, BodyParameters, BodyPartIter, BodyStructure,
    BodyStructureKind, BodyStructureView, DEFAULT_FETCH_RESPONSE_MAX_DEPTH, FetchAddress,
    FetchAddressIter, FetchAddressList, FetchBinaryData, FetchEnvelope, FetchFlag, FetchFlagIter,
    FetchFlags, FetchNString, FetchResponse, FetchResponseItem, FetchResponseItemIter, FetchString,
    FetchStringKind,
};
pub use id::{
    IdLiteralPolicy, IdPair, IdPairIter, IdParameters, IdParametersKind, IdString, IdStringKind,
    IdValue, MAX_ID_FIELD_OCTETS, MAX_ID_PAIRS, MAX_ID_VALUE_OCTETS,
};
pub use list::{
    DEFAULT_LIST_MAX_DEPTH, ListArguments, ListAttribute, ListAttributeIter, ListExtendedItem,
    ListExtendedItemIter, ListMailbox, ListMailboxIter, ListMailboxKind, ListResponse,
    ListReturnOption, ListReturnOptionIter, ListSelectionOption, ListSelectionOptionIter,
};
pub use namespace::{
    DEFAULT_NAMESPACE_MAX_DEPTH, DEFAULT_NAMESPACE_MAX_ITEMS, NamespaceDelimiter,
    NamespaceDescriptor, NamespaceDescriptorIter, NamespaceExtension, NamespaceExtensionIter,
    NamespaceExtensionValueIter, NamespaceGroup, NamespaceResponse, NamespaceString,
    NamespaceStringKind,
};
pub use quota::{
    GetQuotaArguments, GetQuotaRootArguments, MAX_QUOTA_RESOURCES, MAX_QUOTA_ROOTS, QuotaLimit,
    QuotaLimitIter, QuotaResource, QuotaResourceIter, QuotaResourceName, QuotaResponse,
    QuotaRootIter, QuotaRootResponse, SetQuotaArguments,
};
pub use response::{
    ResponseCode, ResponseCodeEncoder, StatusKind, StatusResponse, UntaggedData, parse_untagged,
    parse_untagged_with_max_depth, validate_response_code,
};
pub use search::{
    SavedSearchScope, SavedSearchUpdate, SearchProgram, SearchReturnOption, SearchReturnOptionIter,
};
pub use search_response::{SearchResponse, SearchResultIter};
pub use select::{
    DEFAULT_SELECT_MAX_DEPTH, MAX_SELECT_PARAMETERS, QResyncParameter, SelectArguments,
    SelectParameter, SelectParameterIter, SequenceMatchData,
};
pub use session::{
    ClientSession, IDLE_REISSUE_INTERVAL, ListCorrelation, PendingCommand, SecurityState,
    ServerSession, ServerSessionEvent, SessionEvent, SessionState,
};
pub use sort::{
    DEFAULT_SORT_MAX_DEPTH, MAX_SORT_CRITERIA, SortArguments, SortCriterion, SortCriterionIter,
    SortKey, SortResponse, SortResultIter,
};
pub use status_items::{
    DEFAULT_STATUS_MAX_DEPTH, MailboxStatus, StatusItem, StatusItemIter, StatusItems, StatusValue,
    StatusValueIter, StatusValues,
};
pub use syntax::{
    Sequence, SequenceRange, SequenceRangeIter, SequenceSet, SequenceSetRef, StoreOperation,
};
pub use thread::{
    DEFAULT_THREAD_MAX_DEPTH, DEFAULT_THREAD_MAX_NODES, ThreadAlgorithm, ThreadArguments,
    ThreadEvent, ThreadEventIter, ThreadResponse,
};
pub use types::{
    AuthenticateContinuation, Command, CommandBody, CommandBodyRef, CommandRef, IdleDone, Response,
    Status,
};
