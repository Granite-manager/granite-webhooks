-- Stone finishes each company manages itself, like stone types.
CREATE TABLE stone_finishes (
    id SERIAL PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    company_id INT NOT NULL,
    CONSTRAINT fk_stone_finishes_company
        FOREIGN KEY (company_id)
        REFERENCES company(id)
        ON DELETE CASCADE
);

ALTER TABLE stones MODIFY finishing VARCHAR(255);

INSERT INTO stone_finishes (name, company_id)
SELECT f.name, c.id
  FROM company c
  CROSS JOIN (SELECT 'polished' AS name UNION ALL SELECT 'leathered' UNION ALL SELECT 'honed') f;

-- Any other finish a company's stones already have.
INSERT INTO stone_finishes (name, company_id)
SELECT MIN(TRIM(s.finishing)), s.company_id
  FROM stones s
 WHERE s.finishing IS NOT NULL AND TRIM(s.finishing) <> ''
   AND LOWER(TRIM(s.finishing)) NOT IN ('polished', 'leathered', 'honed')
 GROUP BY s.company_id, LOWER(TRIM(s.finishing));
