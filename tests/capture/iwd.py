"""dbusmock template: minimal iwd (device wlan0, station, a few networks)"""

import dbus
from dbusmock import get_object

BUS_NAME = "net.connman.iwd"
MAIN_OBJ = "/"
SYSTEM_BUS = True
IS_OBJECT_MANAGER = True

DEV = "/net/connman/iwd/0/4"
NETS = [  # path suffix, ssid, type, known, signal (100*dBm)
    ("home_psk", "Home", "psk", True, -4500),
    ("cafe_open", "Cafe Guest", "open", False, -7000),
    ("office_8021x", "Office", "8021x", True, -8000),
    ("neighbor_psk", "Neighbor 5G", "psk", False, -8600),
]


def load(mock, parameters):
    mock.AddObject(DEV, "net.connman.iwd.Device", {
        "Name": "wlan0", "Address": "00:11:22:33:44:55", "Powered": dbus.Boolean(True), "Mode": "station",
    }, [])
    dev = get_object(DEV)
    nets = [(f"{DEV}/{p}", s, t, k, sig) for p, s, t, k, sig in NETS]
    for path, ssid, kind, known, _ in nets:
        props = {"Name": ssid, "Type": kind, "Connected": dbus.Boolean(path == nets[0][0]), "Device": dbus.ObjectPath(DEV)}
        if known:
            props["KnownNetwork"] = dbus.ObjectPath("/net/connman/iwd/" + path.split("/")[-1])
        mock.AddObject(path, "net.connman.iwd.Network", props, [("Connect", "", "", "")])
    ordered = "[" + ", ".join(f"(dbus.ObjectPath('{p}'), dbus.Int16({sig}))" for p, _, _, _, sig in nets) + "]"
    dev.AddProperties("net.connman.iwd.Station", {
        "State": "connected", "Scanning": dbus.Boolean(False), "ConnectedNetwork": dbus.ObjectPath(nets[0][0]),
    })
    dev.AddMethods("net.connman.iwd.Station", [
        ("GetOrderedNetworks", "", "a(on)", "ret = " + ordered),
        ("Scan", "", "", ""),
        ("Disconnect", "", "", ""),
    ])
