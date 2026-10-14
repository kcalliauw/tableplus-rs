-- Seed data for the MariaDB test database.
SET NAMES utf8mb4;
USE shop;

CREATE TABLE customers (
    id         INT AUTO_INCREMENT PRIMARY KEY,
    name       VARCHAR(100) NOT NULL,
    email      VARCHAR(255) NOT NULL UNIQUE,
    vip        BOOLEAN NOT NULL DEFAULT FALSE,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

INSERT INTO customers (name, email, vip, created_at)
SELECT CONCAT('Customer ', seq), CONCAT('c', seq, '@example.org'), seq % 5 = 0,
       TIMESTAMP '2024-01-01 00:00:00' + INTERVAL seq HOUR
FROM seq_1_to_5000;

CREATE TABLE invoices (
    id          INT AUTO_INCREMENT PRIMARY KEY,
    customer_id INT NOT NULL,
    amount      DECIMAL(10, 2) NOT NULL,
    paid        BOOLEAN NOT NULL DEFAULT FALSE,
    FOREIGN KEY (customer_id) REFERENCES customers (id)
);

INSERT INTO invoices (customer_id, amount, paid)
SELECT 1 + seq % 5000, ROUND(10 + (seq * 37) % 990, 2), seq % 2 = 0 FROM seq_1_to_20000;

CREATE VIEW vip_customers AS SELECT * FROM customers WHERE vip;

CREATE TABLE seed_done (ok TINYINT);
INSERT INTO seed_done VALUES (1);
