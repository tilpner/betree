//! This module provides a B<sup>e</sup>-Tree on top of the Data Management
//! Layer.

mod errors;
mod imp;
mod layer;
mod message_action;

pub use self::{
    errors::{Error, ErrorKind},
    imp::{Inner, Node, RangeIterator, Tree},
    layer::{TreeBaseLayer, TreeLayer},
    message_action::{DefaultMessageAction, MessageAction},
};

pub(crate) use self::{imp::MAX_MESSAGE_SIZE, layer::ErasedTreeSync};
