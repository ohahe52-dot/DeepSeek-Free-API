# Railway One Slot

Deploy one Railway service that runs both processes:

- `gateway`: public HTTP/Web UI on `$PORT`
- `worker`: private local process on `127.0.0.1:$WORKER_PORT`

Railway settings:

- Builder: Dockerfile
- Dockerfile path: `Dockerfile`
- Healthcheck path: `/health`

Required env:

```env
ADMIN_TOKEN=change-me
REQUIRE_API_KEY=true
UPSTREAM_BASE_URL=https://provider.example.com
```

Optional env:

```env
WORKER_PORT=18081
ACCOUNTS=email|password
```

After deploy, open the public Railway URL. Add accounts in the Web UI under `Account Import`.
