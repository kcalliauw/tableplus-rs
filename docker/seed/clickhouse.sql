-- Seed data for the ClickHouse test database (deterministic).
CREATE DATABASE IF NOT EXISTS analytics;

CREATE TABLE shop.users (
    id          UInt32,
    email       String,
    full_name   String,
    country     LowCardinality(String) DEFAULT 'US',
    is_active   Bool DEFAULT true,
    balance     Decimal(12, 2) DEFAULT 0,
    nickname    Nullable(String) COMMENT 'Shown instead of the full name',
    created_at  DateTime('UTC') DEFAULT now()
) ENGINE = MergeTree
ORDER BY id
COMMENT 'Registered customers';

INSERT INTO shop.users (id, email, full_name, country, is_active, balance, nickname, created_at)
SELECT
    number + 1,
    concat('user', toString(number + 1), '@example.com'),
    concat('User ', toString(number + 1)),
    ['US', 'BE', 'NL', 'DE', 'FR'][number % 5 + 1],
    number % 7 != 0,
    toDecimal64(number * 3.25, 2),
    if(number % 4 = 0, NULL, concat('nick', toString(number + 1))),
    toDateTime('2024-01-01 00:00:00', 'UTC') + number * 86400
FROM numbers(200);

CREATE TABLE shop.orders (
    id          UInt64,
    user_id     UInt32,
    status      LowCardinality(String),
    total       Decimal(12, 2),
    discount    Nullable(Float64),
    note        Nullable(String),
    items       Array(String),
    tracking    UUID,
    created_at  DateTime('UTC'),
    INDEX idx_status status TYPE set(8) GRANULARITY 4
) ENGINE = MergeTree
PARTITION BY toYYYYMM(created_at)
PRIMARY KEY (user_id, created_at)
ORDER BY (user_id, created_at, id)
SETTINGS index_granularity = 1024;

-- Filled by every insert into orders.
CREATE MATERIALIZED VIEW shop.daily_revenue
ENGINE = SummingMergeTree
ORDER BY day
AS SELECT toDate(created_at) AS day, sum(total) AS revenue, count() AS orders
FROM shop.orders
GROUP BY day;

INSERT INTO shop.orders
SELECT
    number + 1,
    number % 200 + 1,
    ['paid', 'pending', 'shipped', 'refunded'][number % 4 + 1],
    toDecimal64((number % 1000) * 1.25, 2),
    if(number % 3 = 0, NULL, (number % 50) / 100),
    if(number % 5 = 0, NULL, concat('note ', toString(number + 1))),
    arrayMap(i -> concat('sku-', toString(i)), range(number % 4)),
    toUUID(concat('00000000-0000-4000-8000-', leftPad(toString(number + 1), 12, '0'))),
    toDateTime('2024-01-01 00:00:00', 'UTC') + number * 3600
FROM numbers(5000);

CREATE VIEW shop.active_users AS
SELECT id, email, country FROM shop.users WHERE is_active;

-- Text that needs escaping in TabSeparated output.
CREATE TABLE shop.escapes (
    id      UInt8,
    label   String,
    value   Nullable(String),
    raw     String
) ENGINE = MergeTree
ORDER BY id;

INSERT INTO shop.escapes VALUES
    (1, 'tab', 'a\tb', ''),
    (2, 'newline', 'line1\nline2', ''),
    (3, 'backslash', 'C:\\temp\\new', ''),
    (4, 'null', NULL, ''),
    (5, 'backslash N', '\\N', ''),
    (6, 'quote', 'it''s', ''),
    (7, 'unicode', 'héllo wörld ✓', ''),
    (8, 'binary', '', unhex('FF00FE')),
    (9, 'empty', '', '');

CREATE TABLE shop.type_zoo (
    id          UInt8,
    c_int8      Int8,
    c_uint64    UInt64,
    c_int128    Int128,
    c_float32   Float32,
    c_float64   Float64,
    c_decimal   Decimal(18, 4),
    c_bool      Bool,
    c_date      Date,
    c_date32    Date32,
    c_dt64      DateTime64(3, 'UTC'),
    c_enum      Enum8('a' = 1, 'b' = 2),
    c_map       Map(String, UInt8),
    c_tuple     Tuple(String, Int32),
    c_ipv4      IPv4,
    c_fixed     FixedString(4),
    c_nullable  Nullable(Int32),
    c_lc_null   LowCardinality(Nullable(String))
) ENGINE = MergeTree
ORDER BY id;

INSERT INTO shop.type_zoo VALUES
    (1, -8, 18446744073709551615, -170141183460469231731687303715884105728, 1.5, 2.718281828459045,
     12345.6789, true, '2024-02-29', '1900-01-01', '2024-02-29 13:37:00.123', 'b', {'k': 1}, ('x', -1),
     '10.0.0.1', 'abcd', NULL, NULL),
    (2, 0, 0, 0, inf, nan, 0, false, '1970-01-01', '2299-12-31', '1970-01-01 00:00:00.000', 'a', {}, ('', 0),
     '0.0.0.0', 'z', 7, 'seven');

-- No sorting key: paging falls back to the user's sort alone.
CREATE TABLE shop.audit_log (at DateTime, message String) ENGINE = Log;
INSERT INTO shop.audit_log VALUES ('2024-01-01 00:00:00', 'created'), ('2024-01-02 00:00:00', 'updated');

CREATE TABLE analytics.events (
    id          UInt64,
    event_type  LowCardinality(String),
    user_id     UInt32,
    at          DateTime('UTC')
) ENGINE = MergeTree
ORDER BY id;

INSERT INTO analytics.events
SELECT
    number + 1,
    ['signup', 'login', 'view', 'click', 'purchase', 'logout'][number % 6 + 1],
    number % 200 + 1,
    toDateTime('2024-01-01 00:00:00', 'UTC') + number
FROM numbers(250000);

CREATE TABLE shop.seed_done (ok UInt8) ENGINE = TinyLog;
INSERT INTO shop.seed_done VALUES (1);
