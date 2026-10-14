"""mc-spine's connection lifecycle, written from the implementation.

The messages are what a 1.21.1 client sends in each phase - that is the contract the server is
supposed to cover. The transitions are what src/main.rs actually handles. Anything a client may send
that has no transition here is a message the server drops the connection over, which is exactly the
class of failure a real client hits and `wire_smoke.py` does not.

Read from the code:
  - handshake_next_state() picks status (1) or login (2)
  - login: Login Start -> Login Success -> Login Acknowledged (serverbound 0x03)
  - configuration: load_config_packets() -> Finish Configuration -> read_packet() accepts
    Some((3, _)) only, and returns on anything else
  - play: 'conn loop with keep-alive, position, and the action ids
"""

import protogx as p

protocol = p.Protocol(
    states=[
        "handshake",      # no state yet; the next-state field decides
        "status",         # server list ping
        "login",          # login start -> success -> acknowledged
        "configuration",  # registries, then finish -> acknowledged
        "play",           # the game
        "closed",         # disconnected
    ],
    start="handshake",
    terminal=["closed"],
    messages=[
        # serverbound
        "StatusRequest",
        "PingRequest",
        "LoginStart",
        "LoginAcknowledged",
        # 1.21.1 sends these during configuration, in this order, before the ack
        "ClientInformation",
        "SelectKnownPacks",
        "CustomPayload",
        "FinishConfigurationAck",
        # play
        "KeepAlive",
        "SetPlayerPosition",
        "ChatCommand",
        "SwingArm",
        "PlayerAction",
        "Disconnect",
    ],
    transitions=[
        ("handshake", "status", "StatusRequest"),
        ("status", "closed", "PingRequest"),
        ("handshake", "login", "LoginStart"),
        ("login", "configuration", "LoginAcknowledged"),
        # The configuration loops are (0x07) => break, Some(_) => continue, None => continue, so
        # anything else the client sends is tolerated rather than dropped. Modelled as self-loops
        # because the protocol does not leave the state; whether the server *acts* on them is a
        # separate question from whether they break the connection.
        ("configuration", "configuration", "ClientInformation"),
        ("configuration", "configuration", "SelectKnownPacks"),
        ("configuration", "configuration", "CustomPayload"),
        ("configuration", "play", "FinishConfigurationAck"),
        ("configuration", "closed", "Disconnect"),
        # The play loop dispatches these, so they are handled.
        ("play", "play", "KeepAlive"),
        ("play", "play", "SetPlayerPosition"),
        ("play", "play", "ChatCommand"),
        ("play", "play", "SwingArm"),
        ("play", "play", "PlayerAction"),
        ("play", "closed", "Disconnect"),
    ],
    # No requests/responses: this models the serverbound messages the server must handle. What the
    # server sends back is a different claim and would only be checked against a client.
)
