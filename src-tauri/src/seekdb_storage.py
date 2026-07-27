import json
import sys

import pyseekdb


def respond(**payload):
    print(json.dumps(payload, ensure_ascii=False), flush=True)


def open_client(config):
    mode = config["mode"]
    if mode == "seekdb_embedded":
        return pyseekdb.Client(
            path=config["path"],
            database=config.get("database") or "agentseek_desktop",
        )
    return pyseekdb.Client(
        host=config["host"],
        port=int(config.get("port") or 2881),
        tenant=config.get("tenant") or ("sys" if mode == "seekdb_server" else "test"),
        database=config.get("database") or "agentseek_desktop",
        user=config.get("user") or "root",
        password=config.get("password") or "",
    )


def ensure_database(config):
    database = config.get("database") or "agentseek_desktop"
    mode = config["mode"]
    if mode == "seekdb_embedded":
        admin = pyseekdb.AdminClient(path=config["path"])
    else:
        admin = pyseekdb.AdminClient(
            host=config["host"],
            port=int(config.get("port") or 2881),
            tenant=config.get("tenant") or ("sys" if mode == "seekdb_server" else "test"),
            user=config.get("user") or "root",
            password=config.get("password") or "",
        )
    existing = {item.name for item in admin.list_databases()}
    if database not in existing:
        admin.create_database(database)


def open_collection(client, name):
    return client.get_or_create_collection(name)


def initialize_collections(client):
    collections = {
        "instances": open_collection(client, "agentseek_desktop_instances"),
        "vault": open_collection(client, "agentseek_desktop_env_vault"),
        "logs": open_collection(client, "agentseek_desktop_logs"),
    }
    legacy = open_collection(client, "agentseek_desktop_state")
    return collections, legacy


def read_collection(collection):
    rows = []
    offset = 0
    page_size = 1000
    while True:
        result = collection.get(
            limit=page_size,
            offset=offset,
            include=["metadatas"],
        )
        ids = result.get("ids") or []
        metadatas = result.get("metadatas") or []
        rows.extend(zip(ids, metadatas))
        if len(ids) < page_size:
            break
        offset += len(ids)
    return [json.loads(item["payload"]) for _, item in sorted(rows, key=lambda row: row[0])]


def replace_collection(collection, rows, id_field):
    existing = collection.get(include=["metadatas"])
    if existing.get("ids"):
        collection.delete(ids=existing["ids"])
    if not rows:
        return
    ids = []
    metadatas = []
    for index, row in enumerate(rows):
        stable_id = row.get(id_field) if id_field else None
        ids.append(str(stable_id or f"row-{index:08d}"))
        metadatas.append({"payload": json.dumps(row, ensure_ascii=False)})
    collection.upsert(ids=ids, metadatas=metadatas)


def clear_collection(collection):
    existing = collection.get(include=["metadatas"])
    if existing.get("ids"):
        collection.delete(ids=existing["ids"])


def load_domain_data(collections, legacy):
    data = {name: read_collection(collection) for name, collection in collections.items()}
    if not any(data.values()):
        result = legacy.get(ids="singleton", include=["metadatas"])
        metadata = (result.get("metadatas") or [{}])[0] if result.get("ids") else {}
        if metadata.get("payload"):
            data = json.loads(metadata["payload"])
            replace_collection(collections["instances"], data.get("instances", []), "id")
            replace_collection(collections["vault"], data.get("vault", []), None)
            replace_collection(collections["logs"], data.get("logs", []), "id")
            legacy.delete(ids=["singleton"])
    return data


def main():
    with open(sys.argv[1], encoding="utf-8") as config_file:
        config = json.load(config_file)
    # The application database is idempotently provisioned before opening collections.
    ensure_database(config)
    client = open_client(config)
    collections, legacy = initialize_collections(client)
    respond(ok=True, ready=True)
    for raw in sys.stdin:
        try:
            request = json.loads(raw)
            if request["op"] in ("load", "load_core"):
                if request["op"] == "load_core":
                    data = {
                        "instances": read_collection(collections["instances"]),
                        "vault": read_collection(collections["vault"]),
                        "logs": [],
                    }
                    has_data = bool(data["instances"] or data["vault"] or collections["logs"].count())
                    if not has_data:
                        migrated = load_domain_data(collections, legacy)
                        has_data = any(migrated.values())
                        data = {
                            "instances": migrated.get("instances", []),
                            "vault": migrated.get("vault", []),
                            "logs": [],
                        }
                else:
                    data = load_domain_data(collections, legacy)
                    has_data = any(data.values())
                respond(ok=True, payload=json.dumps(data, ensure_ascii=False) if has_data else None)
            elif request["op"] == "save":
                data = json.loads(request["payload"])
                replace_collection(collections["instances"], data.get("instances", []), "id")
                replace_collection(collections["vault"], data.get("vault", []), None)
                replace_collection(collections["logs"], data.get("logs", []), "id")
                respond(ok=True)
            elif request["op"] == "save_core":
                data = json.loads(request["payload"])
                replace_collection(collections["instances"], data.get("instances", []), "id")
                replace_collection(collections["vault"], data.get("vault", []), None)
                respond(ok=True)
            elif request["op"] == "query_logs":
                query = request.get("query") or {}
                before = query.get("beforeSequence")
                after = query.get("afterSequence")
                limit = max(1, min(int(query.get("limit") or 500), 1000))
                all_logs = read_collection(collections["logs"])
                filtered = [
                    entry
                    for entry in all_logs
                    if (before is None or int(entry.get("sequence") or 0) < int(before))
                    and (after is None or int(entry.get("sequence") or 0) > int(after))
                ]
                filtered.sort(
                    key=lambda entry: int(entry.get("sequence") or 0),
                    reverse=after is None,
                )
                groups = {
                    entry.get("instanceId") or f"name:{entry.get('instanceName', '')}"
                    for entry in all_logs
                }
                respond(
                    ok=True,
                    page={
                        "entries": filtered[:limit],
                        "hasMore": len(filtered) > limit,
                        "groupCount": len(groups),
                    },
                )
            elif request["op"] == "max_log_sequence":
                logs = read_collection(collections["logs"])
                respond(ok=True, sequence=max((int(item.get("sequence") or 0) for item in logs), default=0))
            elif request["op"] == "log_count":
                respond(ok=True, count=collections["logs"].count())
            elif request["op"] == "has_completed_deployment":
                instance_id = request["instanceId"]
                completed = any(
                    item.get("instanceId") == instance_id
                    and item.get("category") == "install"
                    and item.get("level") == "success"
                    and item.get("message") == "Instance deployment completed"
                    for item in read_collection(collections["logs"])
                )
                respond(ok=True, completed=completed)
            elif request["op"] == "cleanup_logs":
                now = int(request["now"])
                day = 86400
                runtime_cutoff = now - max(1, int(request["runtimeRetentionDays"])) * day
                deleted_cutoff = now - int(request["deletedRetentionDays"]) * day
                active_ids = {item["id"] for item in read_collection(collections["instances"])}
                logs = read_collection(collections["logs"])
                retained = []
                removed_ids = []
                for item in logs:
                    instance_id = item.get("instanceId")
                    created_at = int(item.get("createdAt") or 0)
                    expired = (
                        (item.get("category") == "runtime" and created_at < runtime_cutoff)
                        or (
                            instance_id is not None
                            and instance_id not in active_ids
                            and created_at < deleted_cutoff
                        )
                    )
                    (removed_ids if expired else retained).append(item["id"] if expired else item)
                max_entries = int(request["maxEntries"])
                if len(retained) > max_entries:
                    remove_limit = len(retained) - max_entries + int(request["batchSize"])
                    candidates = sorted(
                        (
                            item
                            for item in retained
                            if item.get("category") == "runtime"
                            or (
                                item.get("instanceId") is not None
                                and item.get("instanceId") not in active_ids
                            )
                        ),
                        key=lambda item: int(item.get("sequence") or 0),
                    )[:remove_limit]
                    candidate_ids = {item["id"] for item in candidates}
                    removed_ids.extend(candidate_ids)
                if removed_ids:
                    collections["logs"].delete(ids=list(set(removed_ids)))
                respond(ok=True, removed=len(set(removed_ids)))
            elif request["op"] == "clear_logs":
                clear_collection(collections["logs"])
                respond(ok=True)
            elif request["op"] == "append_logs":
                entries = request.get("entries") or []
                if entries:
                    collections["logs"].upsert(
                        ids=[entry["id"] for entry in entries],
                        metadatas=[
                            {"payload": json.dumps(entry, ensure_ascii=False)} for entry in entries
                        ],
                    )
                respond(ok=True)
            elif request["op"] == "append_log":
                entry = request["entry"]
                collections["logs"].upsert(
                    ids=[entry["id"]],
                    metadatas=[{"payload": json.dumps(entry, ensure_ascii=False)}],
                )
                removed_ids = request.get("removedIds") or []
                if removed_ids:
                    collections["logs"].delete(ids=removed_ids)
                respond(ok=True)
            elif request["op"] == "upsert_instance":
                instance = request["instance"]
                collections["instances"].upsert(
                    ids=[instance["id"]],
                    metadatas=[{"payload": json.dumps(instance, ensure_ascii=False)}],
                )
                respond(ok=True)
            elif request["op"] == "delete_instance":
                collections["instances"].delete(ids=[request["instanceId"]])
                respond(ok=True)
            elif request["op"] == "replace_vault":
                replace_collection(collections["vault"], request.get("entries", []), None)
                respond(ok=True)
            elif request["op"] == "delete_logs":
                if request.get("ids"):
                    collections["logs"].delete(ids=request["ids"])
                respond(ok=True)
            elif request["op"] == "ping":
                for collection in collections.values():
                    collection.count()
                respond(ok=True)
            else:
                respond(ok=False, error="unsupported operation")
        except Exception as error:
            respond(ok=False, error=str(error))


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        respond(ok=False, error=str(error))
        raise
