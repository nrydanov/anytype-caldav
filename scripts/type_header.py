#!/usr/bin/env python3
"""Show or set the properties a type puts in the header of its objects.

The REST API cannot do this: a type's header list (`recommendedFeaturedRelations`)
is only reachable over gRPC, and only with a session of full scope. An app key
gets the JsonAPI scope and is refused; the account key of the headless Anytype
(`~/.anytype/config.json`) is accepted.

Needs `grpcurl`, a checkout of anytype-heart for the proto files, and a tunnel
to the headless gRPC port:

    ssh -f -N -L 41010:127.0.0.1:31010 <host>

Usage:
    ANYTYPE_ACCOUNT_KEY=... scripts/type_header.py --space <id> --type <key>
    ANYTYPE_ACCOUNT_KEY=... scripts/type_header.py --space <id> --type <key> \
        --header done,assignee,due_date --apply

Without `--header` it prints the three lists of the type. With it, the header
becomes exactly those keys, in that order; keys that leave the header go to the
top of the sidebar list, so nothing drops off the type. Dry run unless `--apply`.
"""

import argparse
import json
import os
import subprocess
import sys

LISTS = ("recommendedFeaturedRelations", "recommendedRelations", "recommendedHiddenRelations")


class Heart:
    def __init__(self, address, protos, account_key):
        self.base = [
            "grpcurl", "-plaintext", "-import-path", protos,
            "-proto", "pb/protos/service/service.proto",
        ]
        self.address = address
        self.token = self.call("WalletCreateSession", {"accountKey": account_key})["token"]

    def call(self, method, body):
        command = list(self.base)
        if getattr(self, "token", None):
            command += ["-H", f"token: {self.token}"]
        command += ["-d", json.dumps(body), self.address, f"anytype.ClientCommands/{method}"]
        done = subprocess.run(command, capture_output=True, text=True)
        if done.returncode != 0:
            sys.exit(f"{method} failed: {done.stderr.strip()}")
        answer = json.loads(done.stdout or "{}")
        error = answer.get("error", {})
        if error.get("code") not in (None, "NULL", 0):
            sys.exit(f"{method} refused: {error}")
        return answer


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--space", required=True)
    parser.add_argument("--type", required=True, help="the type's key, e.g. task")
    parser.add_argument("--header", help="comma-separated property keys, in order")
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--address", default="127.0.0.1:41010")
    parser.add_argument("--protos", default=os.environ.get("ANYTYPE_HEART", "anytype-heart"))
    args = parser.parse_args()

    heart = Heart(args.address, args.protos, os.environ["ANYTYPE_ACCOUNT_KEY"])

    # Types and relations are objects of the space; find them by their keys.
    def search(layout_filter):
        return heart.call("ObjectSearch", {
            "spaceId": args.space,
            "filters": [layout_filter],
            "keys": ["id", "uniqueKey", "relationKey", "apiObjectKey", "name",
                     "isDeleted", "isArchived"],
        }).get("records", [])

    # Layout 4 is a type. A bundled type is found by its unique key; one made
    # in the space has an opaque unique key and its REST key kept beside it.
    types = search({"RelationKey": "resolvedLayout", "condition": "Equal", "value": 4})
    types = [t for t in types
             if not t.get("isDeleted") and not t.get("isArchived")
             and args.type in (t.get("apiObjectKey"), (t.get("uniqueKey") or "")[3:])]
    if len(types) != 1:
        sys.exit(f"{len(types)} live types have the key {args.type!r}")
    type_id = types[0]["id"]

    relations = search({"RelationKey": "resolvedLayout", "condition": "Equal", "value": 5})
    # A property made by a person has an opaque internal key; the key the REST
    # API shows is kept beside it, and a bundled property has only the first.
    def rest_key(relation):
        return relation.get("apiObjectKey") or relation.get("relationKey")

    by_id = {r["id"]: f"{rest_key(r)} «{r.get('name')}»" for r in relations}
    by_key = {}
    for relation in relations:
        if relation.get("isDeleted") or relation.get("isArchived"):
            continue
        by_key.setdefault(rest_key(relation), []).append(relation["id"])

    view = heart.call("ObjectShow", {"spaceId": args.space, "objectId": type_id})
    details = next(d["details"] for d in view["objectView"]["details"] if d["id"] == type_id)
    current = {name: list(details.get(name) or []) for name in LISTS}
    for name in LISTS:
        print(name, [by_id.get(i, f"?{i[-8:]} (gone)") for i in current[name]])

    if not args.header:
        return

    wanted = []
    for key in [k.strip() for k in args.header.split(",") if k.strip()]:
        ids = by_key.get(key, [])
        if len(ids) != 1:
            sys.exit(f"{len(ids)} live properties have the key {key!r}")
        wanted.append(ids[0])

    leaving = [i for i in current[LISTS[0]] if i not in wanted]
    # What is gone from the space is dropped from the list on the way.
    sidebar = [i for i in leaving + current[LISTS[1]] if i not in wanted and i in by_id]
    sidebar = list(dict.fromkeys(sidebar))
    print("header  ->", [by_id.get(i) for i in wanted])
    print("sidebar ->", [by_id.get(i) for i in sidebar])
    if not args.apply:
        print("dry run: nothing was written; re-run with --apply")
        return

    heart.call("ObjectTypeRecommendedFeaturedRelationsSet",
               {"typeObjectId": type_id, "relationObjectIds": wanted})
    heart.call("ObjectTypeRecommendedRelationsSet",
               {"typeObjectId": type_id, "relationObjectIds": sidebar})
    view = heart.call("ObjectShow", {"spaceId": args.space, "objectId": type_id})
    details = next(d["details"] for d in view["objectView"]["details"] if d["id"] == type_id)
    if list(details.get(LISTS[0]) or []) != wanted:
        sys.exit("the header did not take the new list")
    print("verified")


if __name__ == "__main__":
    main()
