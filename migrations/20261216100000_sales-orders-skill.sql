-- The Confirm sales orders skill (sales orders of quotes marked sold) is an
-- Inventory Manager's by default; companies that changed that position's
-- skills get it too. Other positions get it only when a company ticks it.
UPDATE company_position_sections
   SET skills = CONCAT(skills, IF(skills = '', '', ','), 'confirm_sales_orders')
 WHERE position_id = 12 AND skills IS NOT NULL
   AND FIND_IN_SET('confirm_sales_orders', skills) = 0;
