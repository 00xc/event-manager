// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::io::{AsRawFd, RawFd};

use super::{Error, Result, SubscriberId};
use crate::epoll::EpollWrapper;
use vmm_sys_util::epoll::{ControlOperation, EpollEvent, EventSet};

/// Wrapper over an `epoll::EpollEvent` object.
///
/// When working directly with epoll related methods, the user associates an `u64` wide
/// epoll_data_t object with every event. We want to use fds as identifiers, but at the same time
/// keep the ability to associate opaque data with an event. An `Events` object always contains an
/// fd and an `u32` data member that can be supplied by the user. When registering events with the
/// inner epoll event set, the fd and data members of `Events` as used together to generate the
/// underlying `u64` member of the epoll_data union.
#[derive(Clone, Copy, Debug)]
pub struct Events {
    inner: EpollEvent,
}

impl PartialEq for Events {
    fn eq(&self, other: &Events) -> bool {
        self.fd() == other.fd()
            && self.data() == other.data()
            && self.event_set() == other.event_set()
    }
}

impl Events {
    pub(crate) fn with_inner(inner: EpollEvent) -> Self {
        Self { inner }
    }

    /// Create an empty event set associated with the supplied fd.
    pub fn empty<T: AsRawFd>(source: &T) -> Self {
        Self::empty_raw(source.as_raw_fd())
    }

    /// Create an empty event set associated with the supplied `RawFd` value.
    pub fn empty_raw(fd: RawFd) -> Self {
        Self::new_raw(fd, EventSet::empty())
    }

    /// Create an event set associated with the supplied fd and active events.
    pub fn new<T: AsRawFd>(source: &T, events: EventSet) -> Self {
        Self::new_raw(source.as_raw_fd(), events)
    }

    /// Create an event set associated with the supplied `RawFd` value and active events.
    pub fn new_raw(source: RawFd, events: EventSet) -> Self {
        Self::with_data_raw(source, 0, events)
    }

    /// Create an event set associated with the supplied fd, active events, and data.
    pub fn with_data<T: AsRawFd>(source: &T, data: u32, events: EventSet) -> Self {
        Self::with_data_raw(source.as_raw_fd(), data, events)
    }

    /// Create an event set associated with the supplied `RawFd` value, active events, and data.
    pub fn with_data_raw(source: RawFd, data: u32, events: EventSet) -> Self {
        let inner_data = ((data as u64) << 32) + (source as u64);
        Events {
            inner: EpollEvent::new(events, inner_data),
        }
    }

    /// Return the inner fd value.
    pub fn fd(&self) -> RawFd {
        self.inner.data() as RawFd
    }

    /// Return the inner data value.
    pub fn data(&self) -> u32 {
        (self.inner.data() >> 32) as u32
    }

    /// Return the active event set.
    pub fn event_set(&self) -> EventSet {
        self.inner.event_set()
    }

    /// Return the inner `EpollEvent`.
    pub fn epoll_event(&self) -> EpollEvent {
        self.inner
    }
}

/// This opaque object is associated with an `EventSubscriber`, and allows the addition,
/// modification, and removal of file descriptor events within the inner epoll set of an
/// `EventManager`.
// Right now this is a concrete object, but going further it can be turned into a trait and
// passed around as a trait object.
pub struct EventOps<'a> {
    // Mutable reference to the EpollContext of an EventManager.
    epoll_wrapper: &'a mut EpollWrapper,
    // The id of the event subscriber this object stands for.
    subscriber_id: SubscriberId,
}

impl<'a> EventOps<'a> {
    pub(crate) fn new(epoll_wrapper: &'a mut EpollWrapper, subscriber_id: SubscriberId) -> Self {
        EventOps {
            epoll_wrapper,
            subscriber_id,
        }
    }

    // Apply the provided control operation for the given events on the inner epoll wrapper.
    fn ctl(&self, op: ControlOperation, events: Events) -> Result<()> {
        self.epoll_wrapper
            .epoll
            .ctl(op, events.fd(), events.epoll_event())
            .map_err(Error::Epoll)
    }

    /// Add the provided events to the inner epoll event set.
    pub fn add(&mut self, events: Events) -> Result<()> {
        let fd = events.fd();
        if self.epoll_wrapper.fd_dispatch.contains_key(&fd) {
            return Err(Error::FdAlreadyRegistered);
        }

        self.ctl(ControlOperation::Add, events)?;

        self.epoll_wrapper
            .fd_dispatch
            .insert(fd, self.subscriber_id);

        self.epoll_wrapper
            .subscriber_watch_list
            .entry(self.subscriber_id)
            .or_insert_with(Vec::new)
            .push(fd);

        Ok(())
    }

    /// Submit the provided changes to the inner epoll event set.
    pub fn modify(&self, events: Events) -> Result<()> {
        self.ctl(ControlOperation::Modify, events)
    }

    /// Remove the specified events from the inner epoll event set.
    pub fn remove(&mut self, events: Events) -> Result<()> {
        // TODO: Add some more checks here?
        self.ctl(ControlOperation::Delete, events)?;
        self.epoll_wrapper.fd_dispatch.remove(&events.fd());

        if let Some(watch_list) = self
            .epoll_wrapper
            .subscriber_watch_list
            .get_mut(&self.subscriber_id)
        {
            if let Some(index) = watch_list.iter().position(|&x| x == events.fd()) {
                watch_list.remove(index);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vmm_sys_util::eventfd::EventFd;

    #[test]
    fn test_empty_events() {
        let event_fd = EventFd::new(0).unwrap();

        let events_raw = Events::empty_raw(event_fd.as_raw_fd());
        let events = Events::empty(&event_fd);

        assert_eq!(events, events_raw);

        assert_eq!(events.event_set(), EventSet::empty());
        assert_eq!(events.data(), 0);
        assert_eq!(events.fd(), event_fd.as_raw_fd());
    }

    #[test]
    fn test_events_no_data() {
        let event_fd = EventFd::new(0).unwrap();
        let event_set = EventSet::IN;

        let events_raw = Events::new_raw(event_fd.as_raw_fd(), event_set);
        let events = Events::new(&event_fd, event_set);

        assert_eq!(events_raw, events);

        assert_eq!(events.data(), 0);
        assert_eq!(events.fd(), event_fd.as_raw_fd());
        assert_eq!(events.event_set(), event_set);
    }

    #[test]
    fn test_events_data() {
        let event_fd = EventFd::new(0).unwrap();
        let event_set = EventSet::IN;

        let events_raw = Events::with_data_raw(event_fd.as_raw_fd(), 42, event_set);
        let events = Events::with_data(&event_fd, 43, event_set);

        assert_ne!(events_raw, events);

        assert_eq!(events.data(), 43);
        assert_eq!(events_raw.data(), 42);
    }
}