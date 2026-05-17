# Railway One Slot

Deploy one Railway service that runs both processes:

- `gateway`: public HTTP/Web UI on `$PORT`
- `worker`: private local process on `127.0.0.1:$WORKER_PORT`

Railway settings:

- Builder: Dockerfile
- Dockerfile path from repo root: `Dockerfile.one-slot`
- Healthcheck path: `/health`

Required env:

```env
ADMIN_TOKEN=change-me
REQUIRE_API_KEY=true
UPSTREAM_BASE_URL=https://chat.deepseek.com/api/v0
```

Optional env:

```env
WORKER_PORT=18081
ACCOUNTS=email|password
COMPRESSION_MODEL=deepseek-default
MAX_CONCURRENT_PER_ACCOUNT=1
```

After deploy, open the public Railway URL. Add accounts in the Web UI under `Account Import`.
