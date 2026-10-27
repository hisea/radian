#!/usr/bin/env python3
"""A fake ACP agent for radian-arc's wrapper tests (ADR 0048 decision 7).

It speaks the v1 protocol over stdin/stdout: it answers `initialize` and `session/new`,
sends a few `session/update`s (one `tool_call`, its completion, usage and message chunks),
asks for permission once for a path inside the worktree and once for a path in its state
directory, writes a file in the worktree when asked, and answers `session/prompt` with a
configurable `stopReason`. It honours `session/cancel` by ending the open turn with
`stopReason: cancelled`.
"""

import argparse
import json
import os
import sys


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--state", required=True, help="where the fake records what it saw")
    parser.add_argument("--stop-reason", default="end_turn")
    parser.add_argument("--write-file", default=None, help="a file to leave in the worktree")
    parser.add_argument("--wait-for-cancel", action="store_true")
    args = parser.parse_args()
    os.makedirs(args.state, exist_ok=True)

    session = None
    prompt = None
    stop_reason = args.stop_reason

    def record(name, value):
        with open(os.path.join(args.state, name), "w", encoding="utf-8") as file:
            json.dump(value, file)

    def send(message):
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()

    def update(value):
        send(
            {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": session, "update": value},
            }
        )

    def finish_turn(reason):
        nonlocal prompt
        update(
            {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "title": "Write image.txt",
                "kind": "edit",
                "status": "completed",
            }
        )
        update({"sessionUpdate": "usage_update", "used": 120, "size": 200000})
        # The last contiguous run of message text is this answer; the usage update before it
        # ends the earlier run.
        update(
            {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "the image is ready"},
            }
        )
        send({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": reason}})
        prompt = None

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        method = message.get("method")
        if method == "initialize":
            record("initialize.json", message.get("params"))
            send(
                {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"protocolVersion": 1, "agentCapabilities": {}},
                }
            )
        elif method == "session/new":
            record("session-new.json", message.get("params"))
            session = "fake-session-1"
            send(
                {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"sessionId": session},
                }
            )
        elif method == "session/prompt":
            params = message.get("params") or {}
            text = "".join(
                block.get("text", "")
                for block in params.get("prompt", [])
                if block.get("type") == "text"
            )
            with open(os.path.join(args.state, "brief"), "w", encoding="utf-8") as file:
                file.write(text)
            open(os.path.join(args.state, "prompt-seen"), "w").close()
            prompt = {"id": message["id"], "answered": 0}
            update(
                {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "working"},
                }
            )
            inside = os.path.join(os.getcwd(), "image.txt")
            update(
                {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-1",
                    "title": "Write image.txt",
                    "kind": "edit",
                    "status": "pending",
                    "locations": [{"path": inside}],
                }
            )
            options = [
                {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
            ]
            send(
                {
                    "jsonrpc": "2.0",
                    "id": 1100,
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": session,
                        "toolCall": {
                            "toolCallId": "call-1",
                            "title": "Write image.txt",
                            "kind": "edit",
                            "status": "pending",
                            "locations": [{"path": inside}],
                        },
                        "options": options,
                    },
                }
            )
            outside = os.path.join(args.state, "outside.txt")
            send(
                {
                    "jsonrpc": "2.0",
                    "id": 1101,
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": session,
                        "toolCall": {
                            "toolCallId": "call-2",
                            "title": "Write outside",
                            "kind": "edit",
                            "status": "pending",
                            "locations": [{"path": outside}],
                        },
                        "options": options,
                    },
                }
            )
            if args.wait_for_cancel:
                record("waiting.json", {})
        elif method == "session/cancel":
            open(os.path.join(args.state, "cancel-seen"), "w").close()
            if prompt is not None:
                finish_turn("cancelled")
        elif message.get("id") in (1100, 1101) and "result" in message:
            record("permission-%s.json" % message["id"], message["result"])
            if prompt is not None:
                prompt["answered"] += 1
                if prompt["answered"] == 2 and not args.wait_for_cancel:
                    if args.write_file:
                        path = os.path.join(os.getcwd(), args.write_file)
                        with open(path, "w", encoding="utf-8") as file:
                            file.write("the change\n")
                    finish_turn(stop_reason)
    return 0


if __name__ == "__main__":
    sys.exit(main())
