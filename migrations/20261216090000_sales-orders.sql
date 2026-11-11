-- Sales orders: marking a quote sold makes one for the Inventory Manager, who
-- confirms the slabs it uses (or changes them: another slab, part of one)
-- on the Sales orders page. Confirming sells those slabs (a sale, as Sell
-- slab makes). The sales rep sees none of it.
CREATE TABLE sales_orders (
  id INT NOT NULL AUTO_INCREMENT,
  company_id INT NOT NULL,
  quote_id INT NOT NULL,
  -- The sold option (index in the quote's color options).
  option_index INT NOT NULL,
  customer_id INT NOT NULL,
  deal_id BIGINT NULL,
  -- Who the sale is credited to: the quote's sales rep, else who sold it.
  seller_id INT NULL,
  -- The sold option's total, in dollars.
  price DECIMAL(12, 2) NOT NULL DEFAULT 0,
  status ENUM('pending', 'confirmed', 'cancelled') NOT NULL DEFAULT 'pending',
  -- The sale confirming made (none when no slab was in stock).
  sale_id INT NULL,
  confirmed_by INT NULL,
  confirmed_at DATETIME NULL,
  created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
  PRIMARY KEY (id),
  KEY idx_sales_orders_company (company_id, status, created_at),
  KEY idx_sales_orders_quote (quote_id),
  CONSTRAINT fk_sales_orders_company FOREIGN KEY (company_id) REFERENCES company (id) ON DELETE CASCADE
);

-- One color of the sold option: the rooms using it and the slabs the quote
-- asked for.
CREATE TABLE sales_order_items (
  id INT NOT NULL AUTO_INCREMENT,
  order_id INT NOT NULL,
  position INT NOT NULL DEFAULT 0,
  color VARCHAR(255) NOT NULL,
  product VARCHAR(255) NOT NULL DEFAULT '',
  -- The inventory stone of that color (none: not in inventory).
  stone_id INT NULL,
  rooms VARCHAR(500) NOT NULL DEFAULT '',
  square_feet DECIMAL(10, 2) NOT NULL DEFAULT 0,
  slabs_asked INT NOT NULL DEFAULT 0,
  PRIMARY KEY (id),
  KEY idx_sales_order_items_order (order_id),
  CONSTRAINT fk_sales_order_items_order FOREIGN KEY (order_id) REFERENCES sales_orders (id) ON DELETE CASCADE
);

-- The slabs of one color: suggested when the quote is sold, then as the
-- Inventory Manager sets them. `is_full` 0: only part of the slab.
CREATE TABLE sales_order_slabs (
  id INT NOT NULL AUTO_INCREMENT,
  item_id INT NOT NULL,
  slab_id INT NOT NULL,
  is_full TINYINT(1) NOT NULL DEFAULT 1,
  PRIMARY KEY (id),
  KEY idx_sales_order_slabs_item (item_id),
  KEY idx_sales_order_slabs_slab (slab_id),
  CONSTRAINT fk_sales_order_slabs_item FOREIGN KEY (item_id) REFERENCES sales_order_items (id) ON DELETE CASCADE
);

-- The new Sales orders page is an Inventory Manager's by default; companies
-- that changed that position's pages get it too.
UPDATE company_position_sections
   SET sections = CONCAT(sections, IF(sections = '', '', ','), 'sales_orders')
 WHERE position_id = 12 AND FIND_IN_SET('sales_orders', sections) = 0;
