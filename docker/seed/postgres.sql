-- Seed data for the PostgreSQL test database (deterministic).
CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped', 'delivered', 'cancelled');

CREATE TABLE categories (
    id        serial PRIMARY KEY,
    name      text NOT NULL UNIQUE,
    parent_id integer REFERENCES categories (id) ON DELETE SET NULL
);
COMMENT ON TABLE categories IS 'Product category tree';

CREATE TABLE users (
    id         bigserial PRIMARY KEY,
    email      varchar(255) NOT NULL UNIQUE,
    full_name  text NOT NULL,
    country    char(2) NOT NULL DEFAULT 'US',
    is_active  boolean NOT NULL DEFAULT true,
    balance    numeric(12, 2) NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    metadata   jsonb
);
COMMENT ON COLUMN users.metadata IS 'Free-form profile attributes';
CREATE INDEX users_country_idx ON users (country);
CREATE INDEX users_created_at_idx ON users (created_at DESC);

CREATE TABLE products (
    id          serial PRIMARY KEY,
    sku         varchar(32) NOT NULL UNIQUE,
    name        text NOT NULL,
    category_id integer REFERENCES categories (id),
    price       numeric(10, 2) NOT NULL CHECK (price >= 0),
    stock       integer NOT NULL DEFAULT 0,
    tags        text[] NOT NULL DEFAULT '{}',
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX products_category_idx ON products (category_id);

CREATE TABLE orders (
    id        bigserial PRIMARY KEY,
    user_id   bigint NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    status    order_status NOT NULL DEFAULT 'pending',
    total     numeric(12, 2) NOT NULL DEFAULT 0,
    note      text,
    placed_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX orders_user_idx ON orders (user_id);
CREATE INDEX orders_status_placed_idx ON orders (status, placed_at);

CREATE TABLE order_items (
    order_id   bigint NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    product_id integer NOT NULL REFERENCES products (id),
    quantity   integer NOT NULL CHECK (quantity > 0),
    unit_price numeric(10, 2) NOT NULL,
    PRIMARY KEY (order_id, product_id)
);

CREATE TABLE reviews (
    id         serial PRIMARY KEY,
    product_id integer NOT NULL REFERENCES products (id),
    user_id    bigint NOT NULL REFERENCES users (id),
    rating     smallint NOT NULL CHECK (rating BETWEEN 1 AND 5),
    body       text,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- A table without a primary key (editing must fall back to matching all columns).
CREATE TABLE audit_log (
    happened_at timestamptz NOT NULL DEFAULT now(),
    actor       text,
    action      text NOT NULL,
    detail      jsonb
);

-- One row of (almost) every interesting type, for rendering/round-trip tests.
CREATE TABLE type_zoo (
    id          serial PRIMARY KEY,
    c_uuid      uuid,
    c_inet      inet,
    c_date      date,
    c_time      time,
    c_interval  interval,
    c_bytea     bytea,
    c_json      json,
    c_real      real,
    c_double    double precision,
    c_bool      boolean,
    c_int_arr   integer[],
    c_text      text
);

INSERT INTO categories (name, parent_id) VALUES
    ('Electronics', NULL), ('Books', NULL), ('Home', NULL), ('Garden', NULL), ('Toys', NULL),
    ('Laptops', 1), ('Phones', 1), ('Audio', 1), ('Fiction', 2), ('Non-fiction', 2),
    ('Kitchen', 3), ('Furniture', 3), ('Tools', 4), ('Plants', 4), ('Board games', 5);

INSERT INTO users (email, full_name, country, is_active, balance, created_at, metadata)
SELECT
    'user' || g || '@example.com',
    (ARRAY['Ada','Grace','Alan','Linus','Ken','Barbara','Edsger','Donald','Margaret','Dennis'])[1 + g % 10]
        || ' ' ||
    (ARRAY['Lovelace','Hopper','Turing','Torvalds','Thompson','Liskov','Dijkstra','Knuth','Hamilton','Ritchie'])[1 + (g / 10) % 10],
    (ARRAY['US','CA','GB','DE','FR','JP','BR','IN','AU','ET'])[1 + g % 10],
    g % 7 <> 0,
    round(((g * 7919) % 100000) / 100.0, 2),
    timestamptz '2023-01-01 00:00:00+00' + (g * interval '47 minutes'),
    CASE WHEN g % 3 = 0 THEN jsonb_build_object('plan', 'pro', 'seats', g % 12) ELSE NULL END
FROM generate_series(1, 10000) AS g;

INSERT INTO products (sku, name, category_id, price, stock, tags, created_at)
SELECT
    'SKU-' || lpad(g::text, 5, '0'),
    (ARRAY['Ultra','Mini','Pro','Classic','Smart','Eco','Max','Lite'])[1 + g % 8] || ' ' ||
    (ARRAY['Widget','Gadget','Lamp','Chair','Novel','Drill','Fern','Puzzle','Speaker','Kettle'])[1 + (g / 8) % 10]
        || ' ' || g,
    1 + g % 15,
    round((5 + (g * 37) % 995)::numeric + (g % 100) / 100.0, 2),
    (g * 13) % 250,
    CASE g % 4 WHEN 0 THEN ARRAY['sale'] WHEN 1 THEN ARRAY['new','featured'] ELSE '{}' END,
    timestamptz '2022-06-01 00:00:00+00' + (g * interval '1 day')
FROM generate_series(1, 500) AS g;

INSERT INTO orders (user_id, status, total, note, placed_at)
SELECT
    1 + (g * 7) % 10000,
    (ARRAY['pending','paid','shipped','delivered','cancelled']::order_status[])[1 + g % 5],
    0,
    CASE WHEN g % 11 = 0 THEN 'Gift wrap please' ELSE NULL END,
    timestamptz '2024-01-01 00:00:00+00' + (g * interval '9 minutes')
FROM generate_series(1, 50000) AS g;

INSERT INTO order_items (order_id, product_id, quantity, unit_price)
SELECT o.id, p.pid, 1 + (o.id + p.k) % 4, pr.price
FROM orders o
CROSS JOIN LATERAL (
    SELECT k, 1 + ((o.id * 31 + k * 97) % 500)::int AS pid FROM generate_series(0, (o.id % 3)::int) AS k
) p
JOIN products pr ON pr.id = p.pid
ON CONFLICT DO NOTHING;

UPDATE orders o SET total = s.total
FROM (SELECT order_id, sum(quantity * unit_price) AS total FROM order_items GROUP BY order_id) s
WHERE s.order_id = o.id;

INSERT INTO reviews (product_id, user_id, rating, body, created_at)
SELECT
    1 + (g * 17) % 500,
    1 + (g * 23) % 10000,
    1 + g % 5,
    (ARRAY['Great value!','Would buy again.','Not as described.','Five stars, works perfectly.','Meh.'])[1 + g % 5],
    timestamptz '2024-03-01 00:00:00+00' + (g * interval '13 minutes')
FROM generate_series(1, 20000) AS g;

INSERT INTO audit_log (happened_at, actor, action, detail)
SELECT timestamptz '2024-05-01 00:00:00+00' + g * interval '1 hour',
       (ARRAY['admin','system','ada'])[1 + g % 3],
       (ARRAY['login','export','delete','update'])[1 + g % 4],
       jsonb_build_object('n', g)
FROM generate_series(1, 200) AS g;

INSERT INTO type_zoo (c_uuid, c_inet, c_date, c_time, c_interval, c_bytea, c_json, c_real, c_double, c_bool, c_int_arr, c_text) VALUES
    ('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '192.168.1.10/24', '2024-02-29', '13:37:00', '1 day 2 hours',
     '\xdeadbeef', '{"a": [1, 2, 3]}', 3.14, 2.718281828459045, true, '{1,2,3}', 'héllo wörld ✓'),
    (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
    ('6ba7b810-9dad-11d1-80b4-00c04fd430c8', '::1', '1999-12-31', '23:59:59', '3 months', '\x00ff',
     '"str"', -0.5, 1e300, false, '{}', E'multi\nline\ttext with ''quotes''');

CREATE VIEW active_users AS
    SELECT id, email, full_name, country, created_at FROM users WHERE is_active;

CREATE VIEW order_summary AS
    SELECT o.id, u.email, o.status, o.total, count(oi.*) AS items, o.placed_at
    FROM orders o JOIN users u ON u.id = o.user_id
    LEFT JOIN order_items oi ON oi.order_id = o.id
    GROUP BY o.id, u.email;

CREATE FUNCTION order_total(p_order_id bigint) RETURNS numeric
    LANGUAGE sql STABLE AS
$$ SELECT coalesce(sum(quantity * unit_price), 0) FROM order_items WHERE order_id = p_order_id $$;

CREATE FUNCTION touch_created_at() RETURNS trigger
    LANGUAGE plpgsql AS
$$ BEGIN NEW.created_at := now(); RETURN NEW; END; $$;

-- Second schema with a large table for grid/pagination performance tests.
CREATE SCHEMA analytics;

CREATE TABLE analytics.events (
    id          bigint PRIMARY KEY,
    user_id     bigint NOT NULL,
    event_type  text NOT NULL,
    path        text,
    duration_ms integer,
    occurred_at timestamptz NOT NULL
);

INSERT INTO analytics.events (id, user_id, event_type, path, duration_ms, occurred_at)
SELECT
    g,
    1 + (g * 7919) % 10000,
    (ARRAY['page_view','click','signup','purchase','logout','search'])[1 + g % 6],
    (ARRAY['/','/products','/cart','/checkout','/account','/search'])[1 + (g / 6) % 6],
    (g * 37) % 5000,
    timestamptz '2024-01-01 00:00:00+00' + g * interval '15 seconds'
FROM generate_series(1::bigint, 2000000) AS g;
CREATE INDEX events_user_idx ON analytics.events (user_id);
CREATE INDEX events_type_time_idx ON analytics.events (event_type, occurred_at);

CREATE MATERIALIZED VIEW analytics.daily_events AS
    SELECT date_trunc('day', occurred_at) AS day, event_type, count(*) AS n
    FROM analytics.events GROUP BY 1, 2;

ANALYZE;

CREATE TABLE seed_done (ok boolean);
INSERT INTO seed_done VALUES (true);
