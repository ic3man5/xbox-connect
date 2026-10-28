# Research: how Xbox BLE controllers reconnect (Linux vs. Windows)

Researched 2026-10-28. Covers the Xbox Wireless Controller (Series X|S, PID 0x0B13, firmware 5.x, Bluetooth LE / HID-over-GATT).
Each claim below is marked as **sourced** (link plus quote), **from source code** (file and lines read), or **inference** (my reasoning, not verified).

## Summary

1. **On Linux, the host does the reconnecting, not the controller.** BlueZ marks every bonded LE HID device as *auto-connect* (`MGMT_OP_ADD_DEVICE`, action `0x02`). The kernel then keeps a **background passive scan running with the accept list**. Under `HCI_AUTO_CONN_ALWAYS` it starts `LE Create Connection` as soon as it sees **either** an undirected (`ADV_IND`) or a directed (`ADV_DIRECT_IND`) connectable advertisement from that address. The default background scan runs at **30 ms window / 60 ms interval (50 % duty)**, so a controller that advertises for only a few seconds is still caught. Nothing in the Linux path needs a "fresh session" trick. The host is just listening hard all the time.
2. **When Linux fails, the cause is almost always the bond, not the scan.** The well-documented failure mode for firmware 5.x is this: BlueZ (with `Privacy=off`, the default) **sends no IRK or identity address during SMP key distribution**. The controller treats the bond as incomplete, keeps blinking, and **after about 150 s drops the link and throws the new key away**. Every later reconnect then fails with `PIN or Key Missing (0x06)`. The fixes are `Privacy = device`, `JustWorksRepairing = confirm|always`, and firmware ≥ 5.23. This matches your overnight symptom (b) closely: it connects for a few seconds, the light never goes solid, and nothing is logged as an unpair.
3. **Windows should not need an app at all.** In xpadneo #336, the *same* controller that would not reconnect on Linux "reconnects normally on Windows" on the same PC. Windows distributes its identity information, and its HID-over-GATT stack auto-connects to bonded devices on its own. So if the controller only slow-blinks and gives up, one of two things is likely happening. Either (a) the controller no longer holds a valid bond for this PC, or (b) the Realtek background scan is not catching its short advertising burst. Your observation (a), where a newly opened GattSession connected while the controller was searching, fits (b): an explicit connect request makes Windows scan harder for a while.
4. **Recommendation:** stop treating `MaintainConnection` as "the" mechanism. Run a `BluetoothLEAdvertisementWatcher` (active or passive) filtered to the controller's address. When a **connectable advertisement from the controller** shows up, *immediately* open a new `BluetoothLEDevice` + `GattSession` and trigger an uncached GATT operation. That is the closest Windows equivalent to what the Linux kernel does in `check_pending_le_conn()`. Blindly opening a new session every 3 s is a cruder version of the same idea. It works only by chance overlap and adds constant radio load. Also log the advertisement type and address type the controller uses, because that is the one fact no public source documents.

---

## Q1. How does the controller try to reach its last host after power-on?

**What's documented**

- **Address type: LE Public, Microsoft OUI.** BlueZ logs for this controller show `type LE Public connected` and an `LE Address: 44:16:22:… (Microsoft Corporation)`. The controller uses its public identity address and does not use a resolvable private address. **Sourced:** [bluez/bluez#155](https://github.com/bluez/bluez/issues/155) (RuiGuilherme's and Termuellinator's logs). Because the controller's address is public, the *host* never needs the controller's IRK to recognise it. (**Inference:** this also explains why gbdlin in [xpadneo#336](https://github.com/atar-axis/xpadneo/issues/336) could *delete* the stored `IdentityResolvingKey` section and get reconnects working. A stale IRK entry made the kernel put the device in the resolving list or match it the wrong way, while the controller kept advertising with its public address.)
- **The controller needs the host's identity to commit the bond (firmware 5.x).** From the most detailed public btmon analysis ([xpadneo#449](https://github.com/atar-axis/xpadneo/issues/449), QuantumBBoy, Jul 2026, firmware 5.9): with LE privacy off, "BlueZ distributes no IRK/identity info during SMP". The controller keeps fast-blinking, and about 150 s after it entered pairing mode it "disconnects and discards the key it just negotiated". The xpadneo troubleshooting doc puts it this way: the controller "drops the connection and forgets the new key" because "bluez does not distribute identity information unless LE privacy is enabled" ([TROUBLESHOOTING.md](https://github.com/atar-axis/xpadneo/blob/master/docs/TROUBLESHOOTING.md)).
- **Undirected vs. directed advertising, and how long it advertises:** I found **no public capture or document** that states which PDU type the controller uses when it searches for its last host, or for how long. None of the btmon traces I found (bluez#155, xpadneo#336/#449) show the advertising reports. (**Inference:** the Linux path works whichever type is used, see Q2, so nobody on the Linux side has needed to pin it down. On Windows it matters. If the controller uses *high-duty-cycle directed* advertising, that lasts at most 1.28 s per burst under the Core spec, and a host scanning at low duty will often miss it.) **Action:** measure it with `BluetoothLEAdvertisementWatcher`. `BluetoothLEAdvertisementReceivedEventArgs.AdvertisementType` distinguishes `ConnectableUndirected` from `ConnectableDirected`, and `BluetoothAddressType` gives the address type. Log the timestamps from power-on to give-up to get the search window length.

## Q2. What the host must do to reconnect (Linux reference behaviour)

**From source code.** I read current mainline sources (kernel `net/bluetooth`, BlueZ `master`, fetched 2026-10-28).

1. **BlueZ registers the device for auto-connect.** In `profiles/input/hog.c` the HOG profile is declared with `.auto_connect = true`. `src/device.c: device_set_auto_connect()` calls `adapter_auto_connect_add()`, and in `src/adapter.c` that sends `MGMT_OP_ADD_DEVICE` with `cp.action = 0x02` (*auto-connect remote device*). The function skips devices with a private (RPA) address and devices whose preferred bearer is BR/EDR.
2. **The kernel keeps a passive scan running with the accept list.** `hci_sync.c: hci_passive_scan_sync()` programs the accept list (`hci_update_accept_list_sync`) and starts a **passive** scan. The comment there notes that it deliberately does not use a non-resolvable address, "so that peer devices using direct advertising with our address will be correctly reported". The background scan stays on while `pend_le_conns` is not empty.
3. **It connects on any connectable advertisement.** From `hci_event.c: check_pending_le_conn()`:
   ```c
   case HCI_AUTO_CONN_ALWAYS:
       /* Devices advertising with ADV_IND or ADV_DIRECT_IND
        * are triggering a connection attempt. This means
        * that incoming connections from peripheral device are
        * accepted and also outgoing connections to peripheral
        * devices are established when found.
        */
   ```
   (`HCI_AUTO_CONN_DIRECT` reacts only to `ADV_DIRECT_IND`, but HID devices get `ALWAYS`.) The connect attempt is a normal `LE Create Connection` with `def_le_autoconnect_timeout` = `HCI_LE_CONN_TIMEOUT` = **20 s**. If it fails, the device stays pending and scanning resumes.
4. **Default timing (`include/net/bluetooth/hci_core.h`, `hci_core.c`).**
   - Idle background scan: `le_scan_interval = DISCOV_LE_SCAN_INT_FAST` (0x60 = **60 ms**) and `le_scan_window = DISCOV_LE_SCAN_WIN_FAST` (0x30 = **30 ms**), so **50 % duty**, continuously.
   - While a connection is being established: `DISCOV_LE_SCAN_INT_CONN` / `WIN_CONN` = 60 ms / 60 ms (**100 %**).
   - While the system is suspended: 1.28 s / 11.25 ms (~1 %).
   - BlueZ can override all of these in `main.conf` `[LE]` (`ScanIntervalAutoConnect`, `ScanWindowAutoConnect`, `ScanIntervalConnect`, …) ([main.conf](https://github.com/bluez/bluez/blob/master/src/main.conf)).
5. **`ReconnectAttempts` / `ReconnectIntervals` are not what reconnects the controller.** `main.conf` says they apply "to reconnect after a link lost" (link supervision timeout), for the services in `ReconnectUUIDs` (audio profiles by default). Default: 7 attempts at 1, 2, 4 … 64 s ([main.conf](https://github.com/bluez/bluez/blob/master/src/main.conf)). LE HID reconnects after a power cycle come from the kernel's passive scan described above.
6. **Auth-failure back-off.** If a reconnect fails with `MGMT_DEV_DISCONN_AUTH_FAILURE`, BlueZ disables auto-connect and re-enables it after 1, 2, 4 s, up to a threshold (`device.c`, around line 4133). So a bond that is broken on one side will not loop forever on Linux.

**What this means for the host:** passive scan plus accept list plus auto-connect, at a high duty cycle, all the time. No active connection attempt is needed. The scan just has to be listening when the controller advertises.

## Q3. Known causes of bond loss / rejected keys

| Cause | Evidence | Relevance to your PC |
|---|---|---|
| **Host does not distribute IRK/identity during pairing.** Firmware 5.x treats the bond as incomplete and discards the key about 150 s after pairing. Later reconnects fail with `PIN or Key Missing (0x06)`. | [xpadneo#449](https://github.com/atar-axis/xpadneo/issues/449), [xpadneo TROUBLESHOOTING](https://github.com/atar-axis/xpadneo/blob/master/docs/TROUBLESHOOTING.md). Fix: `Privacy = device`. After the fix the bond file "gained an `[IdentityResolvingKey]` section" and the LED went solid. Confirmed independently on Fedora 44 with a **Realtek** adapter (0bda:4853). | Windows normally distributes identity keys, but this is the mechanism to rule out. **If the LED never went solid after pairing, the controller probably threw the bond away**, which matches symptom (b). |
| **Old controller firmware.** | xpadneo#449: the missing IRK "only matters on the old firmware". 5.9 → 5.23 fixed it. bluez#155 and xpadneo#166 report firmware updates fixing reconnects. Xbox Support also notes that new firmware can *hurt* some hosts and offers a revert ([elevenforum thread on 5.23.5.0](https://www.elevenforum.com/t/issue-with-xbox-wireless-controller-5-23-5-0-firmware-update.34054)). | Check the version in the Xbox Accessories app (USB). Updates are only offered over USB or the dongle, not over Bluetooth ([xpadneo#259](https://github.com/atar-axis/xpadneo/issues/259)). |
| **Just Works re-pairing initiated by the peer is refused.** | BlueZ `JustWorksRepairing` defaults to `never` ("policy to the JUST-WORKS repairing initiated by peer"). With `never`, xpadneo#449 saw a connect/drop loop. `confirm`/`always` fixed it. | **Inference:** the controller may try to re-pair (Just Works) when it thinks its key is stale. The Windows equivalent is whether Windows silently accepts a peer-initiated re-pair for an already bonded device. If it doesn't, you get a connect-then-drop pattern. |
| **Stale/extra IRK on the host side.** | xpadneo#336: reconnects worked after deleting `IdentityResolvingKey` from the BlueZ `info` file. Kernel-list case with the same symptom: [LKML 2020-10](https://lkml.iu.edu/hypermail/linux/kernel/2010.0/04550.html). | Low on Windows. |
| **Pairing button pressed / paired to another host.** | "Pressing the sync button wipes the controller's internal key" (xpadneo#449). Each host generates its own keys, so pairing on another OS or host overwrites the one stored on the controller (dual-boot docs: [bt-dualboot-ng](https://pypi.org/project/bt-dualboot-ng/)). | Make sure nothing else (console, phone, Steam Deck) re-paired it. |
| **Connection parameters not applied → instability.** | xpadneo: the controller wants about a 7.5–11.25 ms interval (100 Hz protocol). BlueZ "never applies the controller's preferred connection parameters". | Drops after connecting, not failure to reconnect. |
| **Host adapter chipset.** | xpadneo: "Some chip sets, e.g. the CSR 85xx or Intel AX200 … do have problems when you try to reconnect the gamepad". Realtek RTL8761B needed its firmware package (bluez#155). | No public report names the RTL8852BE specifically. |
| Secure Connections / key size | Microsoft's accessory guidelines require LE accessories to "use the largest spec- and jurisdiction-allowed encryption key size" ([MS accessory guidelines](https://learn.microsoft.com/en-us/windows-hardware/design/accessory-guidelines/bluetooth-accessory-guidelines/bluetooth-accessory-guidelines-pairing-bonding-connecting)). | I found no Xbox-specific SC or key-size failure. bluez#155 had a single `SMP Confirm value failed` linked to kernel crypto modules. |

**How to tell bond loss from a missed scan on Windows (inference):** if a new session connects while the controller is searching, and the light goes **solid** and input works, the bond is fine and the issue is scan timing. If it connects and drops within about 10 s with the light still blinking, the controller is rejecting the keys, and no reconnect strategy will help. You need a clean remove-and-re-pair, and then you have to find what is wiping the bond.

## Q4. Windows-side issues and workarounds

- **`MaintainConnection` semantics.** Microsoft: "If **GattSession.MaintainConnection** is set to true, then the system waits indefinitely for a connection, and it will connect when the device is available." Separately: "Creating a BluetoothLEDevice object by calling this method alone doesn't (necessarily) initiate a connection. To initiate a connection, set MaintainConnection to true, or call an uncached service discovery method…, or perform a read/write". Also: "disposing of all references to a BluetoothLEDevice object … will trigger an automatic disconnect after a small timeout period." Uncached operations wait 7 s per queued request. "Currently, you can't cancel the connection process." ([GATT client docs](https://learn.microsoft.com/en-us/windows/uwp/devices-sensors/gatt-client), [MaintainConnection](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.genericattributeprofile.gattsession.maintainconnection))
  **Inference:** "waits indefinitely" is implemented as a background (accept-list) connection, and Microsoft does not document its duty cycle. Opening a new session or doing an uncached GATT call queues a *foreground* connect, which almost certainly scans more aggressively for a while. That is consistent with your observations (a) and (c).
- **Windows background scan duty cycle:** **undocumented.** The only Microsoft statement I found is the old Windows 8.x hardware-cert rule that controllers "must use the … LE Scan Interval values as set by Windows at all times", described as a power vs. reconnect-latency trade-off ([devicebuscontroller requirements](https://learn.microsoft.com/en-us/previous-versions/windows/hardware/cert-program/devicebuscontroller-requirements)). A third-party SDK vendor states that Windows 11 24H2 restricted app control of scan parameters ([btframework](https://www.btframework.com/blescanparams.htm)). No public numbers.
- **Same controller works on Windows when Linux fails:** [xpadneo#336](https://github.com/atar-axis/xpadneo/issues/336) (Intel AX200) shows that Windows' native HOGP stack does reconnect this controller after a power cycle on at least some adapters. So your case points at something specific to this PC: the bond, the adapter or driver, or power management.
- **Stuck "Connected" state needing a reboot.** [MS Q&A 4169901](https://learn.microsoft.com/en-us/answers/questions/4169901/xbox-one-series-controller-cant-reconnect-after-be): after an idle disconnect, Settings still shows the controller as connected and the user "had to restart my laptop". An [Intel Community thread](https://community.intel.com/t5/Wireless/The-problem-with-the-game-controller-connected-via-Bluetooth/td-p/1340700) reports that toggling Bluetooth broke reconnects until reboot, later fixed by a Windows update and controller firmware. No root cause is given.
- **Power management.** Common advice that has worked for some Realtek users: untick "Allow the computer to turn off this device to save power" on the Bluetooth adapter **and** on "Bluetooth LE XINPUT compatible input device" ([MS Q&A 3747783](https://learn.microsoft.com/en-us/answers/questions/3747783/xbox-controller-on-pc-keeps-disconnecting-and-reco), [thegeekpage](https://thegeekpage.com/fix-xbox-controller-disconnecting-via-bluetooth-in-windows-11/)). Anecdotal.
- **Firmware regressions on Windows.** A controller is discoverable but stuck at "Connecting"; the suspicion is a new firmware response the PC stack can't handle, and reverting did not help ([MS Q&A 5569932](https://learn.microsoft.com/en-us/answers/questions/5569932/xbox-one-controller-discoverable-but-fails-to-conn)). In another case an Xbox USBDevice driver from Windows Update crashed the PC on BT reconnect ([elevenforum](https://www.elevenforum.com/t/connecting-xbox-controller-via-bt-after-xbox-usbdevice-update-causes-gsod.35218/)).
- **Windows and resolvable private addresses:** a Win10 peripheral using an RPA paired fine but could not reconnect, while one using a static random address worked ([MS Q&A 229207](https://learn.microsoft.com/en-us/answers/questions/229207/windows-10-bluetooth-le-stack-privacy-mode-(privat)). Not relevant here, because the controller uses a public address.
- **RTL8852BE specifically:** I found **no** public report tying this chip to Xbox reconnect failures, and no Realtek release note about it. Realtek adapters do show up in Linux reports (RTL8761B firmware fix in bluez#155; 0bda:4853 in xpadneo#449, fixed by host settings and not the chip).

## Q5. Recommendations for xbox-connect

1. **First, prove whether the bond survives.** After a fresh pair, check that the LED goes **solid**. Then power-cycle and force a connect (new session). Solid LED with input means the bond is good and only scanning is the problem. Blinking then dropping within about 10–150 s means the controller rejected or discarded the keys. In that case the app can't fix it. Check firmware (update or revert via the Xbox Accessories app over USB), make sure no other host re-paired it, and re-pair once.
2. **React to advertisements instead of polling blindly.** Run a `BluetoothLEAdvertisementWatcher` filtered to the controller's Bluetooth address. Active mode is fine, and Windows 11 also has passive mode. On the first **connectable** advertisement (`ConnectableUndirected` or `ConnectableDirected`):
   - drop or dispose any old `BluetoothLEDevice`/`GattSession`,
   - `BluetoothLEDevice.FromBluetoothAddressAsync(addr)` → `GattSession.FromDeviceIdAsync` with `MaintainConnection = true`,
   - kick an **uncached** `GetGattServicesAsync(BluetoothCacheMode.Uncached)` so Windows starts a foreground connect right away.

   This is the Windows analogue of the kernel's `check_pending_le_conn()`: connect the moment the device is seen. It is better than a fixed 3 s loop because it does nothing while the controller is off and acts within one advertising interval while it is searching.
3. **Keep a single long-lived MaintainConnection session as the fallback**, not as the main path. It costs nothing and is what Windows would use if its background scan catches the controller.
4. **If you keep periodic retries, keep them while advertising only, and don't stack them.** Uncached operations queue: each waits about 7 s and "you can't cancel the connection process". A new session every 3 s can therefore build a queue and delay the attempt that would have worked. Only start a new attempt once the previous one has completed or failed, and stop when no advertisement has been seen for, say, 10 s.
5. **Log the advertising facts.** For each advertisement log the advertisement type, address type, RSSI, and time since first seen. Also log `BluetoothLEDevice.ConnectionStatus` changes. This answers the open part of Q1 and shows how long the controller searches before giving up. That window is the deadline the app has to meet.
6. **Treat these as out of the app's scope; document them for the user instead:** adapter and HID power-management checkboxes, the Realtek driver version (from ASUS or Realtek rather than the inbox driver), and controller firmware version or revert.

## Addendum (2026-10-28): off-time evidence, and two hypotheses

**New observations from the app session.**
1. After pairing in Settings, with one `MaintainConnection` session, power-offs shorter than 10 s reconnected.
2. Overnight (off for hours) it failed: it connected once for about 10 s, then dropped, and the LED never went solid.
3. After re-pairing with `DeviceInformationCustomPairing.PairAsync(ConfirmOnly | ConfirmPinMatch)` (default protection level, accepted in `PairingRequested`), it worked while connected. But after a power-off of about 10 s it **never** reconnected, neither with one long-lived session nor with a fresh session every 3 s.

### (a) "Windows' auto-connect / pending connection expires shortly after disconnect"

**Unlikely to be the main cause.**
- Microsoft documents `MaintainConnection` as: the system "waits indefinitely for a connection" ([docs](https://learn.microsoft.com/en-us/windows/uwp/devices-sensors/gatt-client)).
- More decisively, in test 3 the app opened a *new* session every 3 s. Each one queues a fresh connect request, so a "pending connection expired" problem would have been reset every 3 s. A fresh session still failed after a 10 s off-time, when the earlier Settings-paired bond succeeded after the same off-time. So the variable that changed is **the pairing, not the connection management**.
- The leftover version of (a) is (inference) that the repeated sessions queue requests and starve each other. Each connect can take up to ~7 s and "you can't cancel the connection process". That can delay a connect, but it would not explain the Settings-paired bond working with only one session.

### (b) "The controller uses a rotating resolvable private address and Windows has no usable IRK"

**Unlikely for the controller's own address. But the IRK question is still relevant, in the *other* direction.**
- Every public trace of this controller shows its **public** identity address with Microsoft's OUI: `type LE Public`, `LE Address: 44:16:22:… (Microsoft Corporation)` ([bluez#155](https://github.com/bluez/bluez/issues/155)). BlueZ's auto-connect path refuses devices whose address is private (`device_address_is_private()` in `src/device.c`), yet Linux reconnects this controller with that path. That implies the controller reconnects from a non-private address. (Inference, but strong.)
- The IRK that matters in the xpadneo findings is the **host's** IRK, i.e. the PC's identity, which the controller needs. Firmware 5.x treats the bond as incomplete when the host does not distribute its IRK and identity address during SMP key distribution. The controller then keeps blinking and discards the key (about 150 s after pairing started) ([xpadneo#449](https://github.com/atar-axis/xpadneo/issues/449)).
- "It worked while connected" fits this too, because input flows while the controller is still deciding whether to keep the bond. **Check whether the LED went solid after the custom pairing.** If it kept blinking, the controller never committed the bond, so nothing after the power-off could reconnect. This also matches the overnight "connected ~10 s, never solid" symptom.

### Does custom pairing differ from Settings pairing? Does protection level matter?

- **Protection level:** the controller has no display or keyboard, so pairing can only be Just Works, which is unauthenticated (no MITM protection). So `DevicePairingProtectionLevel.EncryptionAndAuthentication` cannot be met and should make pairing *fail* rather than produce a better bond (inference from the SMP rules). `Default` or `Encryption` is right.
- **`ConfirmOnly | ConfirmPinMatch`** is the correct set for Just Works / numeric comparison. Settings pairing does the same exchange.
- **Key distribution:** the [PairAsync docs](https://learn.microsoft.com/en-us/uwp/api/windows.devices.enumeration.deviceinformationcustompairing.pairasync) say nothing about which keys are distributed. I found no public evidence that custom pairing distributes keys differently from Settings, and no WinRT setting that controls IRK distribution. The only way to know on this PC is to compare what Windows stored after each method (below).
- **One real difference to rule out (inference):** with custom pairing, the app controls when `PairAsync` completes and when its own session or device objects are created or disposed. If the link drops or is torn down during or right after key distribution, the controller may end up with a half-finished bond. Then it keeps blinking and discards the key. Two things help: keep the controller connected until the LED goes solid, and do not dispose the `BluetoothLEDevice` during or right after pairing.

### How to check on Windows

**1. What address the controller advertises with, and how (no admin needed).** Use `BluetoothLEAdvertisementWatcher` with `ScanningMode = Active`, unfiltered or filtered by the controller's address. Log these fields from `BluetoothLEAdvertisementReceivedEventArgs` ([docs](https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.advertisement.bluetoothleadvertisementreceivedeventargs)):
- `BluetoothAddress`, `BluetoothAddressType` (Public / Random; Windows 10 2004+), `AdvertisementType`, `IsConnectable`, `IsDirected`, `RawSignalStrengthInDBm`, `Timestamp`.

If `BluetoothAddressType == Random`, the top two bits of the 48-bit address give the subtype:
- `11` = static random
- `01` = resolvable private (RPA). This is the case where an IRK matters.
- `00` = non-resolvable

Note that Windows may already have *resolved* an RPA to the identity address before reporting it, so a Random type with a changing address is the giveaway.

**What the result means:**
- Nothing seen while the controller slow-blinks: it is either not advertising, or sending directed advertisements to an address that is not this adapter's current one. A controller never reports directed advertisements meant for another address. Look at the bond.
- Connectable advertisements seen from its usual public address, yet Windows does not connect: the problem is on the host side.

**2. What Windows stored for the bond (needs SYSTEM, not just admin).**
- The LE keys are in `HKLM\SYSTEM\CurrentControlSet\Services\BTHPORT\Parameters\Keys\<adapter MAC>\<device MAC>`.
- On this PC, `reg query` on `...\Parameters\Keys` from a normal (non-elevated) shell returns `Access is denied`. Even the key's ACL could not be read. Reports say only LocalSystem can read it, so use a SYSTEM-context read-only query. For example, run `reg query` from a one-off SYSTEM scheduled task or `psexec -s` (unverified on this build).
- Value names, from an exported Windows 10 LE bond in [luismaf/bluetooth-dual-boot](https://github.com/luismaf/bluetooth-dual-boot):
  ```
  [...\Keys\<adapter>]           "MasterIRK"=hex:…            (Windows' own local IRK)
  [...\Keys\<adapter>\<device>]  "LTK"=hex:…  "KeyLength"=dword:00000010
                                 "ERand"=hex(b):…  "EDIV"=dword:…
                                 "IRK"=hex:…                  (peer's IRK, if it sent one)
                                 "Address"=hex(b):…  "AddressType"=dword:…
                                 "MasterIRKStatus"=dword:00000001
                                 "AuthReq"=dword:0000002d
  ```

**How to read the values** (only the names and flags; they include the LTK, so don't copy the key bytes into logs or issues):
- `AddressType`: 0 means the controller's identity address is **public**, which is what I expect.
- An `IRK` value on the device means the controller sent its own IRK. If it is absent, the controller does not use privacy.
- `MasterIRKStatus = 1`: by its name, Windows' `MasterIRK` was distributed to this peer. That is inference; it is not documented. This is the value to compare between a Settings pair and a custom pair. If it is 0 or missing after the custom pair, the controller never got the host identity, and the xpadneo#449 failure mode applies.
- `AuthReq` is the SMP AuthReq bitfield. `0x2d` = Bonding (0x01) + MITM (0x04) + Secure Connections (0x08) + CT2 (0x20). Bit 0x08 tells you whether SC pairing was used, and `ERand`/`EDIV` are 0 for SC bonds.
- The `KeyLength` value is the encryption key size; it should be 16.

**Suggested experiment:**
1. Pair in Settings, then dump the value names and flags.
2. Power off for 10 s and for 5 min, recording LED behaviour and watcher output.
3. Remove the device, pair with the app's custom pairing, and repeat steps 1–2.
4. Diff the two sets of values.

## Sources

- Linux kernel `net/bluetooth/hci_event.c` (`check_pending_le_conn`), `hci_sync.c` (`hci_passive_scan_sync`), `hci_core.c`, `include/net/bluetooth/hci_core.h`, `hci.h`: https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/net/bluetooth
- BlueZ `src/main.conf`, `src/device.c`, `src/adapter.c`, `profiles/input/hog.c`: https://github.com/bluez/bluez
- xpadneo troubleshooting: https://github.com/atar-axis/xpadneo/blob/master/docs/TROUBLESHOOTING.md
- xpadneo#449 (IRK/identity root cause, firmware 5.9 vs 5.23): https://github.com/atar-axis/xpadneo/issues/449
- xpadneo#336 (no reconnect after power cycle; works on Windows; fixed by removing the IRK): https://github.com/atar-axis/xpadneo/issues/336
- xpadneo#166, #259: https://github.com/atar-axis/xpadneo/issues/166, https://github.com/atar-axis/xpadneo/issues/259
- bluez#155: https://github.com/bluez/bluez/issues/155
- Microsoft GATT client docs: https://learn.microsoft.com/en-us/windows/uwp/devices-sensors/gatt-client
- `GattSession.MaintainConnection`: https://learn.microsoft.com/en-us/uwp/api/windows.devices.bluetooth.genericattributeprofile.gattsession.maintainconnection
- Microsoft Bluetooth accessory guidelines: https://learn.microsoft.com/en-us/windows-hardware/design/accessory-guidelines/bluetooth-accessory-guidelines/bluetooth-accessory-guidelines-pairing-bonding-connecting
- MS Q&A threads: [4169901](https://learn.microsoft.com/en-us/answers/questions/4169901/xbox-one-series-controller-cant-reconnect-after-be), [5569932](https://learn.microsoft.com/en-us/answers/questions/5569932/xbox-one-controller-discoverable-but-fails-to-conn), [3747783](https://learn.microsoft.com/en-us/answers/questions/3747783/xbox-controller-on-pc-keeps-disconnecting-and-reco), [229207](https://learn.microsoft.com/en-us/answers/questions/229207/windows-10-bluetooth-le-stack-privacy-mode-(privat)
- Intel Community: https://community.intel.com/t5/Wireless/The-problem-with-the-game-controller-connected-via-Bluetooth/td-p/1340700
- Eleven Forum: https://www.elevenforum.com/t/issue-with-xbox-wireless-controller-5-23-5-0-firmware-update.34054, https://www.elevenforum.com/t/connecting-xbox-controller-via-bt-after-xbox-usbdevice-update-causes-gsod.35218/
- Windows 8.x devicebuscontroller requirements: https://learn.microsoft.com/en-us/previous-versions/windows/hardware/cert-program/devicebuscontroller-requirements
- btframework scan parameters note: https://www.btframework.com/blescanparams.htm

**Not found / open questions:** the controller's advertising PDU type and duration when it searches for its host; the Windows background LE scan duty cycle; any RTL8852BE-specific Xbox report. The first can be measured with step 5 above.
