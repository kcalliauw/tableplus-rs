-- Seed data for the SQLite test database (deterministic).
-- Create with: cargo run -p tableplus-core --example make_sqlite -- testdata/shop.sqlite
PRAGMA foreign_keys = ON;

CREATE TABLE categories (
    id        INTEGER PRIMARY KEY,
    name      TEXT NOT NULL UNIQUE,
    parent_id INTEGER REFERENCES categories (id) ON DELETE SET NULL
);

CREATE TABLE users (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    email      TEXT NOT NULL UNIQUE,
    full_name  TEXT NOT NULL,
    country    TEXT NOT NULL DEFAULT 'US',
    is_active  BOOLEAN NOT NULL DEFAULT 1,
    balance    NUMERIC NOT NULL DEFAULT 0,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX users_country_idx ON users (country);

CREATE TABLE products (
    id          INTEGER PRIMARY KEY,
    sku         TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    category_id INTEGER REFERENCES categories (id),
    price       REAL NOT NULL CHECK (price >= 0),
    stock       INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE orders (
    id        INTEGER PRIMARY KEY,
    user_id   INTEGER NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    status    TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','paid','shipped','delivered','cancelled')),
    total     REAL NOT NULL DEFAULT 0,
    note      TEXT,
    placed_at DATETIME NOT NULL
);
CREATE INDEX orders_user_idx ON orders (user_id);

CREATE TABLE order_items (
    order_id   INTEGER NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    product_id INTEGER NOT NULL REFERENCES products (id),
    quantity   INTEGER NOT NULL CHECK (quantity > 0),
    unit_price REAL NOT NULL,
    PRIMARY KEY (order_id, product_id)
);

CREATE TABLE audit_log (
    happened_at DATETIME NOT NULL,
    actor       TEXT,
    action      TEXT NOT NULL
);

CREATE TABLE type_zoo (
    id     INTEGER PRIMARY KEY,
    c_int  INTEGER,
    c_real REAL,
    c_text TEXT,
    c_blob BLOB,
    c_bool BOOLEAN,
    c_date DATE
);

INSERT INTO categories (id, name, parent_id) VALUES
    (1, 'Electronics', NULL), (2, 'Books', NULL), (3, 'Home', NULL), (4, 'Garden', NULL), (5, 'Toys', NULL),
    (6, 'Laptops', 1), (7, 'Phones', 1), (8, 'Audio', 1), (9, 'Fiction', 2), (10, 'Non-fiction', 2);

WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 5000)
INSERT INTO users (email, full_name, country, is_active, balance, created_at)
SELECT
    'user' || n || '@example.com',
    CASE n % 10 WHEN 0 THEN 'Ada' WHEN 1 THEN 'Grace' WHEN 2 THEN 'Alan' WHEN 3 THEN 'Linus' WHEN 4 THEN 'Ken'
                WHEN 5 THEN 'Barbara' WHEN 6 THEN 'Edsger' WHEN 7 THEN 'Donald' WHEN 8 THEN 'Margaret' ELSE 'Dennis' END
        || ' ' ||
    CASE (n / 10) % 5 WHEN 0 THEN 'Lovelace' WHEN 1 THEN 'Hopper' WHEN 2 THEN 'Turing' WHEN 3 THEN 'Knuth' ELSE 'Ritchie' END,
    CASE n % 5 WHEN 0 THEN 'US' WHEN 1 THEN 'CA' WHEN 2 THEN 'GB' WHEN 3 THEN 'DE' ELSE 'JP' END,
    n % 7 <> 0,
    round(((n * 7919) % 100000) / 100.0, 2),
    datetime('2023-01-01 00:00:00', '+' || (n * 47) || ' minutes')
FROM seq;

WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 300)
INSERT INTO products (id, sku, name, category_id, price, stock)
SELECT n, 'SKU-' || substr('00000' || n, -5),
       CASE n % 4 WHEN 0 THEN 'Ultra' WHEN 1 THEN 'Mini' WHEN 2 THEN 'Pro' ELSE 'Classic' END || ' Widget ' || n,
       1 + n % 10, round(5 + (n * 37) % 995 + (n % 100) / 100.0, 2), (n * 13) % 250
FROM seq;

WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 20000)
INSERT INTO orders (id, user_id, status, total, note, placed_at)
SELECT n, 1 + (n * 7) % 5000,
       CASE n % 5 WHEN 0 THEN 'pending' WHEN 1 THEN 'paid' WHEN 2 THEN 'shipped' WHEN 3 THEN 'delivered' ELSE 'cancelled' END,
       0, CASE WHEN n % 11 = 0 THEN 'Gift wrap please' END,
       datetime('2024-01-01 00:00:00', '+' || (n * 9) || ' minutes')
FROM seq;

INSERT OR IGNORE INTO order_items (order_id, product_id, quantity, unit_price)
SELECT o.id, 1 + ((o.id * 31 + k.k * 97) % 300), 1 + (o.id + k.k) % 4, p.price
FROM orders o
JOIN (SELECT 0 AS k UNION ALL SELECT 1 UNION ALL SELECT 2) k ON k.k <= o.id % 3
JOIN products p ON p.id = 1 + ((o.id * 31 + k.k * 97) % 300);

UPDATE orders SET total = (SELECT round(sum(quantity * unit_price), 2) FROM order_items WHERE order_id = orders.id);

WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 100)
INSERT INTO audit_log (happened_at, actor, action)
SELECT datetime('2024-05-01 00:00:00', '+' || n || ' hours'),
       CASE n % 3 WHEN 0 THEN 'admin' WHEN 1 THEN 'system' ELSE 'ada' END,
       CASE n % 4 WHEN 0 THEN 'login' WHEN 1 THEN 'export' WHEN 2 THEN 'delete' ELSE 'update' END
FROM seq;

INSERT INTO type_zoo (c_int, c_real, c_text, c_blob, c_bool, c_date) VALUES
    (42, 3.14, 'héllo wörld ✓', X'DEADBEEF', 1, '2024-02-29'),
    (NULL, NULL, NULL, NULL, NULL, NULL),
    (-1, -0.5, 'multi
line', X'00FF', 0, '1999-12-31');

CREATE VIEW active_users AS SELECT id, email, full_name, country FROM users WHERE is_active;
CREATE VIEW order_summary AS
    SELECT o.id, u.email, o.status, o.total, count(oi.order_id) AS items, o.placed_at
    FROM orders o JOIN users u ON u.id = o.user_id LEFT JOIN order_items oi ON oi.order_id = o.id
    GROUP BY o.id;
