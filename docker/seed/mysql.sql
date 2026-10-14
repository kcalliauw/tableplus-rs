-- Seed data for the MySQL 8 test database (deterministic).
SET NAMES utf8mb4;
CREATE DATABASE IF NOT EXISTS analytics;
GRANT ALL PRIVILEGES ON analytics.* TO 'tp'@'%';
GRANT SHOW_ROUTINE ON *.* TO 'tp'@'%';
FLUSH PRIVILEGES;

USE shop;

CREATE TABLE digits (d TINYINT NOT NULL PRIMARY KEY);
INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);

-- 1..1,000,000
CREATE TABLE seq (n INT NOT NULL PRIMARY KEY);
INSERT INTO seq (n)
SELECT 1 + a.d + b.d * 10 + c.d * 100 + d.d * 1000 + e.d * 10000 + f.d * 100000
FROM digits a, digits b, digits c, digits d, digits e, digits f;

CREATE TABLE categories (
    id        INT AUTO_INCREMENT PRIMARY KEY,
    name      VARCHAR(100) NOT NULL UNIQUE,
    parent_id INT NULL,
    CONSTRAINT fk_categories_parent FOREIGN KEY (parent_id) REFERENCES categories (id) ON DELETE SET NULL
) COMMENT = 'Product category tree';

CREATE TABLE users (
    id         BIGINT AUTO_INCREMENT PRIMARY KEY,
    email      VARCHAR(255) NOT NULL UNIQUE,
    full_name  VARCHAR(200) NOT NULL,
    country    CHAR(2) NOT NULL DEFAULT 'US',
    is_active  TINYINT(1) NOT NULL DEFAULT 1,
    balance    DECIMAL(12, 2) NOT NULL DEFAULT 0,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    metadata   JSON NULL COMMENT 'Free-form profile attributes',
    KEY users_country_idx (country)
);

CREATE TABLE products (
    id          INT AUTO_INCREMENT PRIMARY KEY,
    sku         VARCHAR(32) NOT NULL UNIQUE,
    name        VARCHAR(200) NOT NULL,
    category_id INT NULL,
    price       DECIMAL(10, 2) NOT NULL,
    stock       INT NOT NULL DEFAULT 0,
    created_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_products_category FOREIGN KEY (category_id) REFERENCES categories (id),
    CONSTRAINT chk_price CHECK (price >= 0)
);

CREATE TABLE orders (
    id        BIGINT AUTO_INCREMENT PRIMARY KEY,
    user_id   BIGINT NOT NULL,
    status    ENUM('pending','paid','shipped','delivered','cancelled') NOT NULL DEFAULT 'pending',
    total     DECIMAL(12, 2) NOT NULL DEFAULT 0,
    note      TEXT NULL,
    placed_at DATETIME NOT NULL,
    KEY orders_status_placed_idx (status, placed_at),
    CONSTRAINT fk_orders_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE
);

CREATE TABLE order_items (
    order_id   BIGINT NOT NULL,
    product_id INT NOT NULL,
    quantity   INT NOT NULL,
    unit_price DECIMAL(10, 2) NOT NULL,
    PRIMARY KEY (order_id, product_id),
    CONSTRAINT fk_items_order FOREIGN KEY (order_id) REFERENCES orders (id) ON DELETE CASCADE,
    CONSTRAINT fk_items_product FOREIGN KEY (product_id) REFERENCES products (id),
    CONSTRAINT chk_quantity CHECK (quantity > 0)
);

CREATE TABLE audit_log (
    happened_at DATETIME NOT NULL,
    actor       VARCHAR(50) NULL,
    action      VARCHAR(50) NOT NULL
);

CREATE TABLE type_zoo (
    id        INT AUTO_INCREMENT PRIMARY KEY,
    c_date    DATE NULL,
    c_time    TIME NULL,
    c_blob    BLOB NULL,
    c_json    JSON NULL,
    c_float   FLOAT NULL,
    c_double  DOUBLE NULL,
    c_bit     BIT(8) NULL,
    c_set     SET('a','b','c') NULL,
    c_text    TEXT NULL
);

INSERT INTO categories (name, parent_id) VALUES
    ('Electronics', NULL), ('Books', NULL), ('Home', NULL), ('Garden', NULL), ('Toys', NULL),
    ('Laptops', 1), ('Phones', 1), ('Audio', 1), ('Fiction', 2), ('Non-fiction', 2),
    ('Kitchen', 3), ('Furniture', 3), ('Tools', 4), ('Plants', 4), ('Board games', 5);

INSERT INTO users (email, full_name, country, is_active, balance, created_at, metadata)
SELECT
    CONCAT('user', n, '@example.com'),
    CONCAT(ELT(1 + n % 10, 'Ada','Grace','Alan','Linus','Ken','Barbara','Edsger','Donald','Margaret','Dennis'), ' ',
           ELT(1 + (n DIV 10) % 10, 'Lovelace','Hopper','Turing','Torvalds','Thompson','Liskov','Dijkstra','Knuth','Hamilton','Ritchie')),
    ELT(1 + n % 10, 'US','CA','GB','DE','FR','JP','BR','IN','AU','ET'),
    n % 7 <> 0,
    ROUND(((n * 7919) % 100000) / 100.0, 2),
    TIMESTAMP '2023-01-01 00:00:00' + INTERVAL (n * 47) MINUTE,
    IF(n % 3 = 0, JSON_OBJECT('plan', 'pro', 'seats', n % 12), NULL)
FROM seq WHERE n <= 10000;

INSERT INTO products (sku, name, category_id, price, stock, created_at)
SELECT
    CONCAT('SKU-', LPAD(n, 5, '0')),
    CONCAT(ELT(1 + n % 8, 'Ultra','Mini','Pro','Classic','Smart','Eco','Max','Lite'), ' ',
           ELT(1 + (n DIV 8) % 10, 'Widget','Gadget','Lamp','Chair','Novel','Drill','Fern','Puzzle','Speaker','Kettle'), ' ', n),
    1 + n % 15,
    ROUND(5 + (n * 37) % 995 + (n % 100) / 100.0, 2),
    (n * 13) % 250,
    TIMESTAMP '2022-06-01 00:00:00' + INTERVAL n DAY
FROM seq WHERE n <= 500;

INSERT INTO orders (user_id, status, total, note, placed_at)
SELECT
    1 + (n * 7) % 10000,
    ELT(1 + n % 5, 'pending','paid','shipped','delivered','cancelled'),
    0,
    IF(n % 11 = 0, 'Gift wrap please', NULL),
    TIMESTAMP '2024-01-01 00:00:00' + INTERVAL (n * 9) MINUTE
FROM seq WHERE n <= 50000;

INSERT IGNORE INTO order_items (order_id, product_id, quantity, unit_price)
SELECT o.id, 1 + ((o.id * 31 + k.d * 97) % 500), 1 + (o.id + k.d) % 4, p.price
FROM orders o
JOIN digits k ON k.d <= o.id % 3
JOIN products p ON p.id = 1 + ((o.id * 31 + k.d * 97) % 500);

UPDATE orders o JOIN (
    SELECT order_id, SUM(quantity * unit_price) AS total FROM order_items GROUP BY order_id
) s ON s.order_id = o.id
SET o.total = s.total;

INSERT INTO audit_log (happened_at, actor, action)
SELECT TIMESTAMP '2024-05-01 00:00:00' + INTERVAL n HOUR,
       ELT(1 + n % 3, 'admin','system','ada'),
       ELT(1 + n % 4, 'login','export','delete','update')
FROM seq WHERE n <= 200;

INSERT INTO type_zoo (c_date, c_time, c_blob, c_json, c_float, c_double, c_bit, c_set, c_text) VALUES
    ('2024-02-29', '13:37:00', X'DEADBEEF', '{"a": [1, 2, 3]}', 3.14, 2.718281828459045, b'10100101', 'a,c', 'héllo wörld ✓'),
    (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL);

CREATE VIEW active_users AS
    SELECT id, email, full_name, country, created_at FROM users WHERE is_active = 1;

DELIMITER //
CREATE FUNCTION order_total(p_order_id BIGINT) RETURNS DECIMAL(12, 2)
    READS SQL DATA
BEGIN
    RETURN (SELECT COALESCE(SUM(quantity * unit_price), 0) FROM order_items WHERE order_id = p_order_id);
END //
CREATE PROCEDURE recent_orders(IN p_limit INT)
BEGIN
    SELECT * FROM orders ORDER BY placed_at DESC LIMIT p_limit;
END //
DELIMITER ;

USE analytics;
CREATE TABLE events (
    id          BIGINT PRIMARY KEY,
    user_id     BIGINT NOT NULL,
    event_type  VARCHAR(20) NOT NULL,
    path        VARCHAR(100) NULL,
    duration_ms INT NULL,
    occurred_at DATETIME NOT NULL,
    KEY events_user_idx (user_id)
);
INSERT INTO events (id, user_id, event_type, path, duration_ms, occurred_at)
SELECT n,
       1 + (n * 7919) % 10000,
       ELT(1 + n % 6, 'page_view','click','signup','purchase','logout','search'),
       ELT(1 + (n DIV 6) % 6, '/','/products','/cart','/checkout','/account','/search'),
       (n * 37) % 5000,
       TIMESTAMP '2024-01-01 00:00:00' + INTERVAL (n * 15) SECOND
FROM shop.seq;

USE shop;
CREATE TABLE seed_done (ok TINYINT);
INSERT INTO seed_done VALUES (1);
