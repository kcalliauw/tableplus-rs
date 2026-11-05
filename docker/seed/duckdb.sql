-- Seed data for the DuckDB test database (deterministic), row for row the SQLite shop.
-- Create with: cargo run -p engine-duckdb --example make_duckdb -- testdata/shop.duckdb
--
-- Differences from sqlite.sql: a sequence instead of AUTOINCREMENT, foreign keys without ON DELETE
-- actions (DuckDB rejects them), range() instead of recursive CTEs, // for integer division, the
-- parent categories inserted before their children, and DuckDB's own types in type_zoo.

CREATE SEQUENCE users_id_seq START 1;
CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped', 'delivered', 'cancelled');

CREATE TABLE categories (
    id        INTEGER PRIMARY KEY,
    name      VARCHAR NOT NULL UNIQUE,
    parent_id INTEGER REFERENCES categories (id)
);

CREATE TABLE users (
    id         BIGINT PRIMARY KEY DEFAULT nextval('users_id_seq'),
    email      VARCHAR NOT NULL UNIQUE,
    full_name  VARCHAR NOT NULL,
    country    VARCHAR NOT NULL DEFAULT 'US',
    is_active  BOOLEAN NOT NULL DEFAULT true,
    balance    DECIMAL(12,2) NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX users_country_idx ON users (country);
COMMENT ON TABLE users IS 'Shop customers';

CREATE TABLE products (
    id          INTEGER PRIMARY KEY,
    sku         VARCHAR NOT NULL UNIQUE,
    name        VARCHAR NOT NULL,
    category_id INTEGER REFERENCES categories (id),
    price       DOUBLE NOT NULL CHECK (price >= 0),
    stock       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE orders (
    id        INTEGER PRIMARY KEY,
    user_id   BIGINT NOT NULL REFERENCES users (id),
    status    order_status NOT NULL DEFAULT 'pending',
    total     DOUBLE NOT NULL DEFAULT 0,
    note      VARCHAR,
    placed_at TIMESTAMP NOT NULL
);
CREATE INDEX orders_user_idx ON orders (user_id);

CREATE TABLE order_items (
    order_id   INTEGER NOT NULL REFERENCES orders (id),
    product_id INTEGER NOT NULL REFERENCES products (id),
    quantity   INTEGER NOT NULL CHECK (quantity > 0),
    unit_price DOUBLE NOT NULL,
    PRIMARY KEY (order_id, product_id)
);

CREATE TABLE audit_log (
    happened_at TIMESTAMP NOT NULL,
    actor       VARCHAR,
    action      VARCHAR NOT NULL
);

CREATE TABLE type_zoo (
    id         INTEGER PRIMARY KEY,
    c_int      INTEGER,
    c_real     DOUBLE,
    c_text     VARCHAR,
    c_blob     BLOB,
    c_bool     BOOLEAN,
    c_date     DATE,
    c_hugeint  HUGEINT,
    c_ubigint  UBIGINT,
    c_decimal  DECIMAL(30,10),
    c_uuid     UUID,
    c_time     TIME,
    c_timetz   TIMETZ,
    c_ts_ns    TIMESTAMP_NS,
    c_interval INTERVAL,
    c_list     INTEGER[],
    c_struct   STRUCT(a INTEGER, b VARCHAR),
    c_map      MAP(VARCHAR, INTEGER),
    c_enum     order_status,
    c_json     JSON,
    c_bit      BIT
);

INSERT INTO categories (id, name, parent_id) VALUES
    (1, 'Electronics', NULL), (2, 'Books', NULL), (3, 'Home', NULL), (4, 'Garden', NULL), (5, 'Toys', NULL);
INSERT INTO categories (id, name, parent_id) VALUES
    (6, 'Laptops', 1), (7, 'Phones', 1), (8, 'Audio', 1), (9, 'Fiction', 2), (10, 'Non-fiction', 2);

INSERT INTO users (email, full_name, country, is_active, balance, created_at)
SELECT
    'user' || n || '@example.com',
    CASE n % 10 WHEN 0 THEN 'Ada' WHEN 1 THEN 'Grace' WHEN 2 THEN 'Alan' WHEN 3 THEN 'Linus' WHEN 4 THEN 'Ken'
                WHEN 5 THEN 'Barbara' WHEN 6 THEN 'Edsger' WHEN 7 THEN 'Donald' WHEN 8 THEN 'Margaret' ELSE 'Dennis' END
        || ' ' ||
    CASE (n // 10) % 5 WHEN 0 THEN 'Lovelace' WHEN 1 THEN 'Hopper' WHEN 2 THEN 'Turing' WHEN 3 THEN 'Knuth' ELSE 'Ritchie' END,
    CASE n % 5 WHEN 0 THEN 'US' WHEN 1 THEN 'CA' WHEN 2 THEN 'GB' WHEN 3 THEN 'DE' ELSE 'JP' END,
    n % 7 <> 0,
    round(((n * 7919) % 100000) / 100.0, 2),
    TIMESTAMP '2023-01-01 00:00:00' + to_minutes(n * 47)
FROM range(1, 5001) r(n)
ORDER BY n;

INSERT INTO products (id, sku, name, category_id, price, stock)
SELECT n, 'SKU-' || lpad(n::VARCHAR, 5, '0'),
       CASE n % 4 WHEN 0 THEN 'Ultra' WHEN 1 THEN 'Mini' WHEN 2 THEN 'Pro' ELSE 'Classic' END || ' Widget ' || n,
       1 + n % 10, round(5 + (n * 37) % 995 + (n % 100) / 100.0, 2), (n * 13) % 250
FROM range(1, 301) r(n);

INSERT INTO orders (id, user_id, status, total, note, placed_at)
SELECT n, 1 + (n * 7) % 5000,
       CASE n % 5 WHEN 0 THEN 'pending' WHEN 1 THEN 'paid' WHEN 2 THEN 'shipped' WHEN 3 THEN 'delivered' ELSE 'cancelled' END,
       0, CASE WHEN n % 11 = 0 THEN 'Gift wrap please' END,
       TIMESTAMP '2024-01-01 00:00:00' + to_minutes(n * 9)
FROM range(1, 20001) r(n);

INSERT OR IGNORE INTO order_items (order_id, product_id, quantity, unit_price)
SELECT o.id, 1 + ((o.id * 31 + k.k * 97) % 300), 1 + (o.id + k.k) % 4, p.price
FROM orders o
JOIN (SELECT 0 AS k UNION ALL SELECT 1 UNION ALL SELECT 2) k ON k.k <= o.id % 3
JOIN products p ON p.id = 1 + ((o.id * 31 + k.k * 97) % 300);

UPDATE orders SET total = (SELECT round(sum(quantity * unit_price), 2) FROM order_items WHERE order_id = orders.id);

INSERT INTO audit_log (happened_at, actor, action)
SELECT TIMESTAMP '2024-05-01 00:00:00' + to_hours(n),
       CASE n % 3 WHEN 0 THEN 'admin' WHEN 1 THEN 'system' ELSE 'ada' END,
       CASE n % 4 WHEN 0 THEN 'login' WHEN 1 THEN 'export' WHEN 2 THEN 'delete' ELSE 'update' END
FROM range(1, 101) r(n);

INSERT INTO type_zoo VALUES
    (1, 42, 3.14, 'héllo wörld ✓', from_hex('DEADBEEF'), true, '2024-02-29',
     170141183460469231731687303715884105727, 18446744073709551615, 12345678901234567890.0123456789,
     'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '13:37:00.123456', '13:37:00+02', '2024-02-29 13:37:00.123456789',
     INTERVAL '1 year 2 months 3 days 04:05:06.789', [1, 2, NULL], {'a': 1, 'b': 'x'}, MAP {'k1': 1, 'k2': 2},
     'paid', '{"tags": ["a", "b"], "n": 1}', '10110'),
    (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
     NULL, NULL),
    (3, -1, -0.5, 'multi
line', from_hex('00FF'), false, '1999-12-31', -1, 0, -0.5, '00000000-0000-0000-0000-000000000000', '00:00:00',
     '23:59:59-05:30', '1970-01-01 00:00:00', INTERVAL '-1 day', [], {'a': NULL, 'b': 'it''s'}, MAP {}, 'cancelled',
     '[]', '0');

CREATE VIEW active_users AS SELECT id, email, full_name, country FROM users WHERE is_active;
CREATE VIEW order_summary AS
    SELECT o.id, u.email, o.status, o.total, count(oi.order_id) AS items, o.placed_at
    FROM orders o JOIN users u ON u.id = o.user_id LEFT JOIN order_items oi ON oi.order_id = o.id
    GROUP BY o.id, u.email, o.status, o.total, o.placed_at;

CREATE MACRO order_value(qty, price) AS round(qty * price, 2);
CREATE MACRO orders_of(uid) AS TABLE SELECT * FROM orders WHERE user_id = uid;
