#![deny(unused_results)]

use async_io::Async;
use evdev_rs::enums::{EventCode, EventType, EV_SYN, EV_KEY, EV_REL};
use evdev_rs::{InputEvent, UInputDevice};
use futures::{ready, Stream, StreamExt as _, TryStreamExt as _};
use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

pub trait UInputExt {
    fn inject_event(&self, event_code: EventCode, value: i32) -> std::io::Result<()>;

    fn inject_key_press(&self, btn: EV_KEY) -> std::io::Result<()> {
        self.inject_event(EventCode::EV_KEY(btn), 1)?;
        self.inject_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
        self.inject_event(EventCode::EV_KEY(btn), 0)?;
        self.inject_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
        Ok(())
    }

    fn inject_key_syn(&self, key: EV_KEY, value: i32) -> std::io::Result<()> {
        self.inject_event(EventCode::EV_KEY(key), value)?;
        self.inject_event(EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0)?;
        Ok(())
    }
}

impl UInputExt for UInputDevice {
    fn inject_event(&self, event_code: EventCode, value: i32) -> std::io::Result<()> {
        self.write_event(&InputEvent {
            event_code,
            value,
            time: evdev_rs::TimeVal {
                tv_sec: 0,
                tv_usec: 0,
            },
        })
    }
}

struct Device(evdev_rs::Device);

impl AsRawFd for Device {
    fn as_raw_fd(&self) -> RawFd {
        self.0.file().as_raw_fd()
    }
}

pub struct AsyncDevice(Async<Device>);

impl futures::Stream for AsyncDevice {
    type Item = Result<InputEvent, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // XXX This logic is hideous because libevdev's `next_event` function will read all
        // available events from the fd and buffer them internally, so when the fd becomes readable
        // it's necessary to continue from libevdev until the buffer is exhausted before the fd
        // will signal readable again.
        Poll::Ready(Some(if self.has_event_pending() {
            self.next_event(evdev_rs::ReadFlag::NORMAL)
                .map(|(_, event)| event)
        } else {
            match ready!(self.0.poll_readable(cx)) {
                Ok(()) => {
                    match self
                        .next_event(evdev_rs::ReadFlag::NORMAL)
                        .map(|(_, event)| event)
                    {
                        Ok(event) => Ok(event),
                        Err(e) => {
                            if e.kind() == std::io::ErrorKind::WouldBlock {
                                return self.poll_next(cx);
                            } else {
                                Err(e)
                            }
                        }
                    }
                }
                Err(e) => Err(e),
            }
        }))
    }
}

impl AsyncDevice {
    pub fn new<P: AsRef<Path>>(path: P) -> std::io::Result<Self> {
        File::open(path)
            .and_then(|file| evdev_rs::Device::new_from_file(file))
            .and_then(|device| Async::new(Device(device)))
            .map(AsyncDevice)
    }

    pub fn grab(&mut self, grab: evdev_rs::GrabMode) -> std::io::Result<()> {
        self.0.get_mut().0.grab(grab)
    }

    pub fn next_event(
        &self,
        flags: evdev_rs::ReadFlag,
    ) -> std::io::Result<(evdev_rs::ReadStatus, InputEvent)> {
        self.0.get_ref().0.next_event(flags)
    }

    pub fn has_event_pending(&self) -> bool {
        self.0.get_ref().0.has_event_pending()
    }
}

#[derive(Error, Debug)]
pub enum IdentifyError {
    #[error("glob pattern error")]
    PatternError(#[from] glob::PatternError),
    #[error("glob iterator error")]
    GlobError(#[from] glob::GlobError),
    #[error("failed to create async device")]
    AsyncDeviceNew(#[source] std::io::Error),
    #[error("combined device event stream ended")]
    EventStreamEnded,
    #[error("error when yielding an event")]
    ReadEvent(#[source] std::io::Error),
}

fn all_devices() -> Result<impl Stream<Item = std::io::Result<(PathBuf, InputEvent)>>, IdentifyError> {
    let paths = glob::glob("/dev/input/event*")?.into_iter().collect::<Result<Vec<_>, _>>()?;
    let devices = paths
        .into_iter()
        .map(|path| {
            AsyncDevice::new(&path)
                .map(|stream| stream.map(move |event| event.map(|event| (path.clone(), event))))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(IdentifyError::AsyncDeviceNew)?;
    Ok(futures::stream::select_all(devices))
}

pub async fn identify_keyboard() -> Result<PathBuf, IdentifyError> {
    let mut streams = all_devices()?;
    loop {
        let (
            path,
            InputEvent {
                time: _,
                event_code,
                value,
            },
        ) = streams
            .try_next()
            .await
            .map_err(IdentifyError::ReadEvent)?
            .ok_or_else(|| IdentifyError::EventStreamEnded)?;
        if let EventCode::EV_KEY(k) = event_code {
            if k as u32 >= EV_KEY::KEY_RESERVED as u32 &&
                k as u32 <= EV_KEY::KEY_MICMUTE as u32 && value == 0 {
                    return Ok(path);
            }
        }
    }
}

pub async fn identify_mkb() -> Result<(PathBuf, PathBuf), IdentifyError> {
    let (mut keeb_path, mut mouse_path) = (None, None);
    let mut streams = all_devices()?;
    loop {
        let (
            path,
            InputEvent {
                time: _,
                event_code,
                value,
            },
        ) = streams
            .try_next()
            .await
            .map_err(IdentifyError::ReadEvent)?
            .ok_or_else(|| IdentifyError::EventStreamEnded)?;
        match event_code {
            EventCode::EV_KEY(EV_KEY::BTN_LEFT)
            | EventCode::EV_KEY(EV_KEY::BTN_RIGHT)
            | EventCode::EV_KEY(EV_KEY::BTN_MIDDLE)
            | EventCode::EV_KEY(EV_KEY::BTN_EXTRA)
            | EventCode::EV_KEY(EV_KEY::BTN_SIDE)
            | EventCode::EV_REL(EV_REL::REL_X)
            | EventCode::EV_REL(EV_REL::REL_Y)
            | EventCode::EV_REL(EV_REL::REL_WHEEL)
            | EventCode::EV_REL(EV_REL::REL_HWHEEL) => {
                if mouse_path.is_none() {
                    mouse_path = Some(path);
                }
            }
            // TODO this is grossly inaccurate
            EventCode::EV_KEY(_) => {
                if value == 0 && keeb_path.is_none() {
                    keeb_path = Some(path);
                }
            }
            _ => {}
        }
        if let (Some(keeb_path), Some(mouse_path)) = (&keeb_path, &mouse_path) {
            return Ok((keeb_path.clone(), mouse_path.clone()));
        }
    }
}

pub trait DeviceWrapperExt: evdev_rs::DeviceWrapper {
    fn enable_codes(&self, start: EventCode, end: EventCode) -> std::io::Result<()> {
        for code in start.iter() {
            self.enable(&code)?;
            if code == end {
                return Ok(());
            }
        }
        Ok(())
    }

    fn enable_keys(&self) -> std::io::Result<()> {
        self.enable(&EventType::EV_KEY)?;
        self.enable_codes(
            EventCode::EV_KEY(EV_KEY::KEY_RESERVED),
            EventCode::EV_KEY(EV_KEY::KEY_MICMUTE),
        )?;
        Ok(())
    }

    fn enable_mouse(&self) -> std::io::Result<()> {
        self.enable(&EventType::EV_REL)?;
        self.enable(&EventType::EV_KEY)?;
        self.enable_codes(EventCode::EV_KEY(EV_KEY::BTN_LEFT), EventCode::EV_KEY(EV_KEY::BTN_EXTRA))?;
        self.enable_codes(EventCode::EV_REL(EV_REL::REL_X), EventCode::EV_REL(EV_REL::REL_MAX))?;
        Ok(())
    }

    fn enable_gamepad(&self) -> std::io::Result<()> {
        self.enable(&EventType::EV_KEY)?;
        self.enable(&EventType::EV_ABS)?;
        self.enable_codes(EventCode::EV_KEY(EV_KEY::BTN_0), EventCode::EV_KEY(EV_KEY::BTN_9))?;
        self.enable_codes(EventCode::EV_KEY(EV_KEY::BTN_TRIGGER), EventCode::EV_KEY(EV_KEY::BTN_THUMBR))?;
        self.enable_codes(EventCode::EV_KEY(EV_KEY::BTN_DPAD_UP), EventCode::EV_KEY(EV_KEY::BTN_DPAD_RIGHT))?;
        Ok(())
    }
}

impl<D: evdev_rs::DeviceWrapper> DeviceWrapperExt for D {}
