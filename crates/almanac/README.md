# Almanac · 个人日历与通讯录

Almanac 是 Steadholme 主权基础设施中的**个人日历 + 通讯录**服务，部署在 `cal.w33d.xyz`。
它是服务端渲染的 Rust + axum 应用，遵循全estate 统一的 SHARED TEMPLATE：异步 `Store`
抽象（内存 + PostgreSQL 两种实现）、企业级 Steadholme UI、CSRF 防护、健康检查子命令、多阶段
非 root 镜像。

## 设计要点

- **零自建登录**：Almanac 位于 Sluice 网关的 `auth=sso` 路由之后。网关完成 OIDC 浏览器登录，
  剥离客户端传入的任何 `X-Auth-*`，再注入校验过的 `X-Auth-Subject` / `X-Auth-Email`。
  服务**信任**这两个头部：
  - 每条 event / contact 的**所有者**取自 `X-Auth-Subject`（稳定 id），**绝不**来自任何
    客户端字段；
  - `X-Auth-Email` 仅用于右上角「已登录」展示。
- **按所有者隔离**：所有读写都以 `owner_sub` 为作用域，两个用户彼此看不到、也改不了对方的数据。
  Postgres 层的 upsert 还带有 `WHERE <table>.owner_sub = EXCLUDED.owner_sub` 的二次所有权护栏。
- **时间一律按 UTC 处理**（v1 刻意保持简单，无时区/夏令时边界）。时间戳统一存为 epoch 毫秒
  （`BIGINT`）。
- **CSRF 双提交**：每个含表单的 GET 会铸造一个随机 token，写入 JS-free 的
  `__Host-almanac_csrf` Cookie，并放进每个表单的隐藏字段；POST 时要求二者常数时间比对一致。
- **不可信文本一律转义**：所有用户输入（标题、地点、备注、姓名等）在输出时统一 HTML 转义，
  杜绝存储型 XSS。

## 路由

| 方法 + 路径 | 说明 | 鉴权 |
|---|---|---|
| `GET /healthz` | 存活探针（纯文本 `ok`），容器 HEALTHCHECK 使用 | 公开 |
| `GET /` | 本月日历网格 + 即将到来的日程（`?y=&m=` 切换月份） | sso |
| `GET /new` | 新建事件表单（`?date=YYYY-MM-DD` 预填某天） | sso |
| `POST /new` | 创建事件 | sso |
| `GET /edit/{id}` | 编辑事件表单 | sso |
| `POST /edit/{id}` | 更新事件 | sso |
| `POST /delete/{id}` | 删除事件 | sso |
| `GET /contacts` | 通讯录（地址簿）+ 新增表单 | sso |
| `POST /contacts/new` | 新增联系人 | sso |
| `GET /contacts/edit/{id}` | 编辑联系人表单 | sso |
| `POST /contacts/edit/{id}` | 更新联系人 | sso |
| `POST /contacts/delete/{id}` | 删除联系人 | sso |

## 数据模型（数据库 `almanac`）

便携标准 SQL（`TEXT` / `BIGINT` / `BOOLEAN`，`PRIMARY KEY` / `NOT NULL` / `DEFAULT`，
`INSERT .. ON CONFLICT`，普通索引），运行时查询、无编译期宏，构建无需数据库；同样的语句日后
可在 FusionDB 的 pgwire 上原样运行。

```sql
events(
  id TEXT PRIMARY KEY, owner_sub TEXT NOT NULL, title TEXT NOT NULL,
  starts_at BIGINT NOT NULL, ends_at BIGINT NOT NULL,
  all_day BOOLEAN NOT NULL DEFAULT FALSE,
  location TEXT NOT NULL DEFAULT '', notes TEXT NOT NULL DEFAULT '',
  created_at BIGINT NOT NULL
)  -- idx_events_owner_start (owner_sub, starts_at)

contacts(
  id TEXT PRIMARY KEY, owner_sub TEXT NOT NULL, name TEXT NOT NULL,
  email TEXT NOT NULL DEFAULT '', phone TEXT NOT NULL DEFAULT '',
  notes TEXT NOT NULL DEFAULT '', created_at BIGINT NOT NULL
)  -- idx_contacts_owner_name (owner_sub, name)
```

> 可空文本列统一改为 `NOT NULL DEFAULT ''`，使 Rust 侧保持纯 `String`、无 `Option` 边界情况。

## 配置（环境变量）

| 变量 | 默认值 | 说明 |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:8960` | 监听地址（内部端口 8960） |
| `ALMANAC_STORE` | `memory` | `memory`（无数据库）或 `postgres` |
| `DATABASE_URL` | — | `ALMANAC_STORE=postgres` 时必填 |

## 构建与运行

```bash
# 默认（数据库无关）测试套件
cargo test
cargo clippy --all-targets -- -D warnings

# 构建镜像并冒烟
docker build -t steadholme/almanac:dev .
docker run --rm -p 127.0.0.1:8960:8960 steadholme/almanac:dev   # 然后 curl /healthz

# PostgreSQL 集成测试（需外部 Postgres）
docker run --rm -d --name almanac-testpg -e POSTGRES_PASSWORD=pw -e POSTGRES_DB=almanac \
  -p 127.0.0.1:55480:5432 postgres:18-alpine
TEST_DATABASE_URL=postgres://postgres:pw@127.0.0.1:55480/almanac \
  cargo test --test pg_store -- --nocapture
docker rm -f almanac-testpg
```

## 部署接入

- 数据库：`almanac`（共享 Postgres，host `postgres`，user `holdfast`）。
- 内部端口：`8960`，仅内网（`http://almanac:8960`），不对外发布端口。
- Sluice 路由：host `cal.w33d.xyz`，`path_prefix /`，upstream `http://almanac:8960`，`auth=sso`。
- Portal 磁贴名：`Calendar`；Beacon 组件名：`Calendar`（`http://almanac:8960/healthz`）。

## 已推迟（DEFER）

以下能力本期不实现，留待后续：

- **CalDAV / CardDAV 同步端点**（与外部日历/通讯录客户端互通）。
- **重复事件**（recurring events / RRULE）。
- **提醒 / 通知**（reminders）。
- 每用户时区（当前一律按 UTC 展示与计算）。
