-- postgres
[op 0] autocommit
  CREATE INDEX s3_t_name_raw ON "s3_t" ("name");
[op 1] autocommit
  CREATE INDEX CONCURRENTLY s3_t_name_hash ON "s3_t" USING hash ("name");
[op 2] tx
  UPDATE "s3_t" SET "name" = 'x' WHERE 1 = 0;
-- mysql
[op 0] autocommit
  CREATE INDEX s3_t_name_raw ON `s3_t` (`name`);
[op 2] tx
  UPDATE `s3_t` SET `name` = 'x' WHERE 1 = 0;
-- sqlite
[op 0] autocommit
  CREATE INDEX s3_t_name_raw ON "s3_t" ("name");
[op 2] tx
  UPDATE "s3_t" SET "name" = 'x' WHERE 1 = 0;
