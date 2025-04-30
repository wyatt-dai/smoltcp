use std::cell::RefCell;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::vec::Vec;

use crate::phy::{self, sys, Device, DeviceCapabilities, Medium};
use crate::time::Instant;
use crate::wire::*;
use core::task::Waker;
use core::future::poll_fn;
use core::task::Poll;

/// A virtual TUN (IP) or TAP (Ethernet) interface.
#[derive(Debug)]
pub struct TunTapInterface {
    lower: Rc<RefCell<sys::TunTapInterfaceDesc>>,
    mtu: usize,
    medium: Medium,
    rx_waker: Waker,
}

impl AsRawFd for TunTapInterface {
    fn as_raw_fd(&self) -> RawFd {
        self.lower.borrow().as_raw_fd()
    }
}

impl TunTapInterface {
    /// Attaches to a TUN/TAP interface called `name`, or creates it if it does not exist.
    ///
    /// If `name` is a persistent interface configured with UID of the current user,
    /// no special privileges are needed. Otherwise, this requires superuser privileges
    /// or a corresponding capability set on the executable.
    pub fn new(name: &str, medium: Medium) -> io::Result<TunTapInterface> {
        let lower = sys::TunTapInterfaceDesc::new(name, medium)?;
        let mtu = lower.interface_mtu()?;
        Ok(TunTapInterface {
            lower: Rc::new(RefCell::new(lower)),
            mtu,
            medium,
            rx_waker: None, // ✅ 新增：初始化为 None
        })
    }

    /// Attaches to a TUN/TAP interface specified by file descriptor `fd`.
    ///
    /// On platforms like Android, a file descriptor to a tun interface is exposed.
    /// On these platforms, a TunTapInterface cannot be instantiated with a name.
    pub fn from_fd(fd: RawFd, medium: Medium, mtu: usize) -> io::Result<TunTapInterface> {
        let lower = sys::TunTapInterfaceDesc::from_fd(fd, mtu)?;
        Ok(TunTapInterface {
            lower: Rc::new(RefCell::new(lower)),
            mtu,
            medium,
            rx_waker: None, // ✅ 新增：初始化为 None
        })
    }

}

impl Device for TunTapInterface {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        DeviceCapabilities {
            max_transmission_unit: self.mtu,
            medium: self.medium,
            ..DeviceCapabilities::default()
        }
    }

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut lower = self.lower.borrow_mut();
        let mut buffer = vec![0; self.mtu];
        match lower.recv(&mut buffer[..]) {
            Ok(size) => {
                buffer.resize(size, 0);
                // ✅ 触发接收事件唤醒
                self.rx_waker.wake(); 
                let rx = RxToken { buffer };
                let tx = TxToken {
                    lower: self.lower.clone(),
                };
                self.rx_waker.wake(); 
                Some((rx, tx))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => None,
            Err(err) => panic!("{}", err),
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken {
            lower: self.lower.clone(),
        })
    }
    // 注册接收Waker（由异步poll_ingress调用）
    #[cfg(feature = "async")]
    fn register_rx_waker(&mut self, waker: &Waker) {
        self.rx_waker.register(waker);
    }

}

#[doc(hidden)]
pub struct RxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for RxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer[..])
    }
}

#[doc(hidden)]
pub struct TxToken {
    lower: Rc<RefCell<sys::TunTapInterfaceDesc>>,
}

impl phy::TxToken for TxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut lower = self.lower.borrow_mut();
        let mut buffer = vec![0; len];
        let result = f(&mut buffer);
        match lower.send(&buffer[..]) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                net_debug!("phy: tx failed due to WouldBlock")
            }
            Err(err) => panic!("{}", err),
        }
        result
    }
}
