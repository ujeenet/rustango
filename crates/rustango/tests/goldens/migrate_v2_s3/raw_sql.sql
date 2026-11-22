-- postgres
[op 0] tx
  CREATE INDEX s3_t_name_raw ON "s3_t" ("name");
[op 1] autocommit
  CREATE INDEX CONCURRENTLY s3_t_name_hash ON "s3_t" USING hash ("name");
-- mysql
[op 0] autocommit
  CREATE INDEX s3_t_name_raw ON `s3_t` (`name`);
-- sqlite
[op 0] tx
  CREATE INDEX s3_t_name_raw ON "s3_t" ("name");
