import asyncio
import sys
import socket
import argparse
import logging
import time
from collections import defaultdict
from bleak import BleakClient, BleakScanner

JDY_CHAR_UUID = "0000ffe1-0000-1000-8000-00805f9b34fb"
DEFAULT_TARGET_MAC = "A8:76:B4:9C:A4:C6"
ECHO_TTL_SECONDS = 0.40  # 400ms window for hardware UART echo

logging.basicConfig(
    level=logging.INFO,
    format='[%(asctime)s] [%(levelname)s] %(message)s',
    datefmt='%H:%M:%S'
)
logger = logging.getLogger("NeothesiaBTBridge")

class NeothesiaBTBridge:
    def __init__(self, tcp_port: int, target_mac: str):
        self.tcp_port = tcp_port
        self.target_mac = target_mac.upper()
        self.tcp_reader = None
        self.tcp_writer = None
        self.ble_client = None
        self.running = True
        # (key_type, key) -> list of send timestamps
        self.echo_timestamps = defaultdict(list)

    def register_expected_echo(self, key_type: int, key: int):
        now = time.monotonic()
        # Clean expired timestamps first
        self.echo_timestamps[(key_type, key)] = [
            t for t in self.echo_timestamps[(key_type, key)] if now - t < ECHO_TTL_SECONDS
        ]
        self.echo_timestamps[(key_type, key)].append(now)

    def is_echo_and_consume(self, key_type: int, key: int) -> bool:
        now = time.monotonic()
        timestamps = [t for t in self.echo_timestamps[(key_type, key)] if now - t < ECHO_TTL_SECONDS]
        if timestamps:
            timestamps.pop(0)
            self.echo_timestamps[(key_type, key)] = timestamps
            return True
        self.echo_timestamps[(key_type, key)] = []
        return False

    async def run(self):
        logger.info(f"Connecting to Neothesia TCP Server on 127.0.0.1:{self.tcp_port}...")
        for attempt in range(20):
            try:
                self.tcp_reader, self.tcp_writer = await asyncio.open_connection('127.0.0.1', self.tcp_port)
                logger.info("[OK] Connected to Neothesia core TCP socket.")
                break
            except Exception as e:
                logger.info(f"Waiting for Neothesia TCP server... ({attempt+1}/20)")
                await asyncio.sleep(1)
        else:
            logger.error("[FAIL] Could not connect to Neothesia TCP socket.")
            return

        while self.running:
            try:
                logger.info(f"Scanning / Connecting to Piano Bluetooth [{self.target_mac}]...")
                device = await BleakScanner.find_device_by_address(self.target_mac, timeout=10.0)
                if not device:
                    logger.warning(f"Device {self.target_mac} not found in scan, trying direct connect...")
                    device = self.target_mac

                async with BleakClient(device) as client:
                    self.ble_client = client
                    logger.info(f"[OK] Bluetooth connected to Piano ({client.address})!")
                    self.echo_timestamps.clear()

                    def ble_notify_callback(sender, data: bytearray):
                        if not self.tcp_writer or self.tcp_writer.is_closing():
                            return

                        filtered_bytes = bytearray()
                        i = 0
                        while i < len(data):
                            status = data[i]
                            msg_type = status & 0xF0

                            if msg_type in (0x80, 0x90, 0xA0, 0xB0, 0xE0):
                                if i + 2 < len(data):
                                    key = data[i+1]
                                    vel = data[i+2]
                                    is_on = (msg_type == 0x90 and vel > 0)
                                    key_type = 0x90 if is_on else 0x80

                                    if msg_type in (0x80, 0x90) and self.is_echo_and_consume(key_type, key):
                                        logger.debug(f"[ECHO FILTERED] Dropped echo for Note {key} (on={is_on})")
                                    else:
                                        filtered_bytes.extend(data[i:i+3])
                                    i += 3
                                else:
                                    break
                            elif msg_type in (0xC0, 0xD0):
                                if i + 1 < len(data):
                                    filtered_bytes.extend(data[i:i+2])
                                    i += 2
                                else:
                                    break
                            else:
                                filtered_bytes.append(status)
                                i += 1

                        if filtered_bytes:
                            self.tcp_writer.write(filtered_bytes)

                    await client.start_notify(JDY_CHAR_UUID, ble_notify_callback)
                    logger.info("[OK] BLE Notification active. Forwarding Piano -> Neothesia.")

                    async def tcp_to_ble_loop():
                        while self.running and client.is_connected:
                            data = await self.tcp_reader.read(128)
                            if not data:
                                logger.info("Neothesia closed TCP connection. Exiting bridge.")
                                self.running = False
                                break
                            try:
                                # Parse outgoing MIDI stream to register expected echoes
                                i = 0
                                while i < len(data):
                                    status = data[i]
                                    msg_type = status & 0xF0
                                    if msg_type in (0x80, 0x90, 0xA0, 0xB0, 0xE0):
                                        if i + 2 < len(data):
                                            if msg_type in (0x80, 0x90):
                                                key = data[i+1]
                                                vel = data[i+2]
                                                is_on = (msg_type == 0x90 and vel > 0)
                                                key_type = 0x90 if is_on else 0x80
                                                self.register_expected_echo(key_type, key)
                                            elif msg_type == 0xB0:
                                                cc_num = data[i+1]
                                                if cc_num == 123:  # All Notes Off
                                                    self.echo_timestamps.clear()
                                            i += 3
                                        else:
                                            break
                                    elif msg_type in (0xC0, 0xD0):
                                        i += 2
                                    else:
                                        i += 1

                                await client.write_gatt_char(JDY_CHAR_UUID, data, response=True)
                            except Exception as write_err:
                                logger.error(f"BLE write error: {write_err}")

                    await tcp_to_ble_loop()

            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Bluetooth connection error: {e}")
                if not self.running:
                    break
                logger.info("Retrying Bluetooth connection in 3 seconds...")
                await asyncio.sleep(3)

        if self.tcp_writer and not self.tcp_writer.is_closing():
            self.tcp_writer.close()
            await self.tcp_writer.wait_closed()
        logger.info("Bridge terminated gracefully.")

def main():
    parser = argparse.ArgumentParser(description="Neothesia Direct Bluetooth MIDI Bridge")
    parser.add_argument("--port", type=int, default=48888, help="TCP port of Neothesia")
    parser.add_argument("--target", type=str, default=DEFAULT_TARGET_MAC, help="Target JDY-33 MAC Address")
    args = parser.parse_args()

    bridge = NeothesiaBTBridge(args.port, args.target)
    try:
        asyncio.run(bridge.run())
    except KeyboardInterrupt:
        pass

if __name__ == "__main__":
    main()
