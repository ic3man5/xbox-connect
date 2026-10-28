//! Bluetooth LE advertisement scanner, used by re-pairing to see a controller enter pairing mode
//! (it then advertises as discoverable) before anything about its existing pairing is touched.

use std::sync::mpsc::{Receiver, channel};

use windows::Devices::Bluetooth::Advertisement::{
    BluetoothLEAdvertisementFlags, BluetoothLEAdvertisementReceivedEventArgs, BluetoothLEAdvertisementType,
    BluetoothLEAdvertisementWatcher, BluetoothLEAdvertisementWatcherStatus, BluetoothLEScanningMode,
};
use windows::Foundation::TypedEventHandler;
use windows::core::Result;

/// One advertisement (or scan response) heard.
pub struct Sighting {
    pub addr: u64,
    pub kind: BluetoothLEAdvertisementType,
    /// Advertised with the general/limited discoverable flag, i.e. it wants to be paired.
    pub discoverable: bool,
    /// Local name, if this packet carried one (often only the scan response does).
    pub name: String,
    pub rssi: i16,
}

pub struct Scanner {
    watcher: BluetoothLEAdvertisementWatcher,
    rx: Receiver<Sighting>,
}

impl Scanner {
    /// Starts an active scan; with `only`, packets from other addresses are dropped in the callback.
    pub fn start(only: Option<u64>) -> Result<Self> {
        let watcher = BluetoothLEAdvertisementWatcher::new()?;
        watcher.SetScanningMode(BluetoothLEScanningMode::Active)?; // active: scan responses carry the name
        let (tx, rx) = channel();
        watcher.Received(&TypedEventHandler::<
            BluetoothLEAdvertisementWatcher,
            BluetoothLEAdvertisementReceivedEventArgs,
        >::new(move |_, args| {
            if let Ok(a) = args.ok() {
                let addr = a.BluetoothAddress()?;
                if only.is_some_and(|o| o != addr) {
                    return Ok(());
                }
                let ad = a.Advertisement()?;
                let flags = ad.Flags().and_then(|f| f.Value()).map_or(0, |f| f.0);
                let wanted = BluetoothLEAdvertisementFlags::GeneralDiscoverableMode.0
                    | BluetoothLEAdvertisementFlags::LimitedDiscoverableMode.0;
                let _ = tx.send(Sighting {
                    addr,
                    kind: a.AdvertisementType()?,
                    discoverable: flags & wanted != 0,
                    name: ad.LocalName().map(|n| n.to_string()).unwrap_or_default(),
                    rssi: a.RawSignalStrengthInDBm()?,
                });
            }
            Ok(())
        }))?;
        watcher.Start()?;
        Ok(Self { watcher, rx })
    }

    /// Everything heard since the last call. Restarts the scan if Windows stopped it.
    pub fn drain(&self) -> Vec<Sighting> {
        if matches!(
            self.watcher.Status(),
            Ok(BluetoothLEAdvertisementWatcherStatus::Aborted | BluetoothLEAdvertisementWatcherStatus::Stopped)
        ) {
            let _ = self.watcher.Start();
        }
        self.rx.try_iter().collect()
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        let _ = self.watcher.Stop();
    }
}

pub fn kind_name(k: BluetoothLEAdvertisementType) -> &'static str {
    match k {
        BluetoothLEAdvertisementType::ConnectableUndirected => "connectable",
        BluetoothLEAdvertisementType::ConnectableDirected => "directed",
        BluetoothLEAdvertisementType::ScannableUndirected => "scannable",
        BluetoothLEAdvertisementType::NonConnectableUndirected => "non-connectable",
        BluetoothLEAdvertisementType::ScanResponse => "scan response",
        BluetoothLEAdvertisementType::Extended => "extended",
        _ => "other",
    }
}
