-- Admin › Quotes › Automation "Sales order on a sale": marking a quote Sold
-- makes its sales order for the Inventory Manager. Off for every company.
ALTER TABLE company ADD COLUMN auto_sales_order TINYINT(1) NOT NULL DEFAULT 0;
