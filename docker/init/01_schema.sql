-- sqsh test environment: schema definitions
-- All identifiers and data are in English to avoid character encoding issues.

SET NAMES utf8mb4;
SET character_set_client = utf8mb4;

-- ============================================================
-- testdb (main test database, created by MYSQL_DATABASE env)
-- ============================================================
USE testdb;

CREATE TABLE IF NOT EXISTS users (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    username    VARCHAR(64)     NOT NULL,
    email       VARCHAR(128)    NOT NULL,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_users_email (email)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS products (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    name        VARCHAR(128)    NOT NULL,
    price       DECIMAL(10,2)   NOT NULL,
    category    VARCHAR(64)     NOT NULL,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS orders (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    user_id     INT UNSIGNED    NOT NULL,
    product_id  INT UNSIGNED    NOT NULL,
    quantity    INT UNSIGNED    NOT NULL DEFAULT 1,
    total       DECIMAL(12,2)   NOT NULL,
    ordered_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_orders_user_id    (user_id),
    KEY idx_orders_product_id (product_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

-- ============================================================
-- ecommerce
-- ============================================================
CREATE DATABASE IF NOT EXISTS ecommerce
    CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

USE ecommerce;

GRANT ALL PRIVILEGES ON ecommerce.* TO 'testuser'@'%';

CREATE TABLE IF NOT EXISTS customers (
    id              INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    first_name      VARCHAR(64)     NOT NULL,
    last_name       VARCHAR(64)     NOT NULL,
    email           VARCHAR(128)    NOT NULL,
    country         VARCHAR(64)     NOT NULL DEFAULT 'US',
    registered_at   DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_customers_email (email)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS categories (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    name        VARCHAR(64)     NOT NULL,
    parent_id   INT UNSIGNED    NULL,
    PRIMARY KEY (id),
    KEY idx_categories_parent (parent_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS items (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    name        VARCHAR(128)    NOT NULL,
    description TEXT,
    price       DECIMAL(10,2)   NOT NULL,
    category_id INT UNSIGNED    NOT NULL,
    stock       INT UNSIGNED    NOT NULL DEFAULT 0,
    PRIMARY KEY (id),
    KEY idx_items_category (category_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS transactions (
    id              INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    customer_id     INT UNSIGNED    NOT NULL,
    item_id         INT UNSIGNED    NOT NULL,
    quantity        INT UNSIGNED    NOT NULL DEFAULT 1,
    amount          DECIMAL(12,2)   NOT NULL,
    purchased_at    DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_transactions_customer (customer_id),
    KEY idx_transactions_item     (item_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS reviews (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    customer_id INT UNSIGNED    NOT NULL,
    item_id     INT UNSIGNED    NOT NULL,
    rating      TINYINT UNSIGNED NOT NULL DEFAULT 3,
    comment     TEXT,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_reviews_customer (customer_id),
    KEY idx_reviews_item     (item_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

-- ============================================================
-- blog
-- ============================================================
CREATE DATABASE IF NOT EXISTS blog
    CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

USE blog;

GRANT ALL PRIVILEGES ON blog.* TO 'testuser'@'%';

CREATE TABLE IF NOT EXISTS authors (
    id      INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    name    VARCHAR(128)    NOT NULL,
    bio     TEXT,
    email   VARCHAR(128)    NOT NULL,
    PRIMARY KEY (id),
    UNIQUE KEY uq_authors_email (email)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS posts (
    id              INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    author_id       INT UNSIGNED    NOT NULL,
    title           VARCHAR(256)    NOT NULL,
    body            MEDIUMTEXT,
    status          ENUM('draft','published','archived') NOT NULL DEFAULT 'draft',
    published_at    DATETIME        NULL,
    PRIMARY KEY (id),
    KEY idx_posts_author (author_id),
    KEY idx_posts_status (status)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS comments (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    post_id     INT UNSIGNED    NOT NULL,
    author_name VARCHAR(128)    NOT NULL,
    body        TEXT            NOT NULL,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_comments_post (post_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS tags (
    id      INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    name    VARCHAR(64)     NOT NULL,
    PRIMARY KEY (id),
    UNIQUE KEY uq_tags_name (name)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS post_tags (
    post_id INT UNSIGNED    NOT NULL,
    tag_id  INT UNSIGNED    NOT NULL,
    PRIMARY KEY (post_id, tag_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

-- ============================================================
-- analytics
-- ============================================================
CREATE DATABASE IF NOT EXISTS analytics
    CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

USE analytics;

GRANT ALL PRIVILEGES ON analytics.* TO 'testuser'@'%';

-- events: large table for streaming/pagination demo (~1M rows)
CREATE TABLE IF NOT EXISTS events (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    event_type  VARCHAR(64)     NOT NULL,
    user_id     INT UNSIGNED    NOT NULL,
    page_url    VARCHAR(256)    NOT NULL,
    referrer    VARCHAR(256)    NULL,
    ip_address  VARCHAR(45)     NOT NULL,
    user_agent  VARCHAR(256)    NOT NULL,
    created_at  DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_events_event_type  (event_type),
    KEY idx_events_user_id     (user_id),
    KEY idx_events_created_at  (created_at)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS sessions (
    id          BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    user_id     INT UNSIGNED    NOT NULL,
    started_at  DATETIME        NOT NULL,
    ended_at    DATETIME        NULL,
    page_count  INT UNSIGNED    NOT NULL DEFAULT 0,
    PRIMARY KEY (id),
    KEY idx_sessions_user_id (user_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS page_views (
    id                  BIGINT UNSIGNED NOT NULL AUTO_INCREMENT,
    session_id          BIGINT UNSIGNED NOT NULL,
    url                 VARCHAR(256)    NOT NULL,
    duration_seconds    INT UNSIGNED    NOT NULL DEFAULT 0,
    created_at          DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    KEY idx_page_views_session (session_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

-- ============================================================
-- inventory
-- ============================================================
CREATE DATABASE IF NOT EXISTS inventory
    CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

USE inventory;

GRANT ALL PRIVILEGES ON inventory.* TO 'testuser'@'%';

CREATE TABLE IF NOT EXISTS warehouses (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    name        VARCHAR(128)    NOT NULL,
    location    VARCHAR(128)    NOT NULL,
    capacity    INT UNSIGNED    NOT NULL DEFAULT 0,
    PRIMARY KEY (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS products (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    sku         VARCHAR(64)     NOT NULL,
    name        VARCHAR(128)    NOT NULL,
    description TEXT,
    unit_price  DECIMAL(10,2)   NOT NULL,
    PRIMARY KEY (id),
    UNIQUE KEY uq_products_sku (sku)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS stock_levels (
    id              INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    warehouse_id    INT UNSIGNED    NOT NULL,
    product_id      INT UNSIGNED    NOT NULL,
    quantity        INT UNSIGNED    NOT NULL DEFAULT 0,
    last_updated    DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (id),
    UNIQUE KEY uq_stock_warehouse_product (warehouse_id, product_id),
    KEY idx_stock_product (product_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS shipments (
    id              INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    warehouse_id    INT UNSIGNED    NOT NULL,
    product_id      INT UNSIGNED    NOT NULL,
    quantity        INT UNSIGNED    NOT NULL,
    shipped_at      DATETIME        NOT NULL DEFAULT CURRENT_TIMESTAMP,
    destination     VARCHAR(128)    NOT NULL,
    PRIMARY KEY (id),
    KEY idx_shipments_warehouse (warehouse_id),
    KEY idx_shipments_product   (product_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

-- ============================================================
-- hr_system
-- ============================================================
CREATE DATABASE IF NOT EXISTS hr_system
    CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;

USE hr_system;

GRANT ALL PRIVILEGES ON hr_system.* TO 'testuser'@'%';

CREATE TABLE IF NOT EXISTS departments (
    id          INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    name        VARCHAR(128)    NOT NULL,
    -- manager_id references employees; populated after employees are inserted
    manager_id  INT UNSIGNED    NULL,
    PRIMARY KEY (id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS employees (
    id              INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    first_name      VARCHAR(64)     NOT NULL,
    last_name       VARCHAR(64)     NOT NULL,
    email           VARCHAR(128)    NOT NULL,
    department_id   INT UNSIGNED    NOT NULL,
    position        VARCHAR(128)    NOT NULL,
    salary          DECIMAL(12,2)   NOT NULL,
    hired_at        DATE            NOT NULL,
    PRIMARY KEY (id),
    UNIQUE KEY uq_employees_email (email),
    KEY idx_employees_department (department_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS projects (
    id              INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    name            VARCHAR(128)    NOT NULL,
    department_id   INT UNSIGNED    NOT NULL,
    budget          DECIMAL(14,2)   NOT NULL,
    started_at      DATE            NOT NULL,
    deadline        DATE            NOT NULL,
    PRIMARY KEY (id),
    KEY idx_projects_department (department_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

CREATE TABLE IF NOT EXISTS time_entries (
    id              INT UNSIGNED    NOT NULL AUTO_INCREMENT,
    employee_id     INT UNSIGNED    NOT NULL,
    project_id      INT UNSIGNED    NOT NULL,
    hours           DECIMAL(5,2)    NOT NULL,
    work_date       DATE            NOT NULL,
    description     VARCHAR(256)    NULL,
    PRIMARY KEY (id),
    KEY idx_time_entries_employee (employee_id),
    KEY idx_time_entries_project  (project_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;

FLUSH PRIVILEGES;
