-- postgres
[op 0] autocommit
  CREATE INDEX CONCURRENTLY "s3_t_a_idx" ON "s3_t" ("a");
[op 1] autocommit
  CREATE UNIQUE INDEX CONCURRENTLY "s3_t_b_uq" ON "s3_t" ("b");
[op 2] autocommit
  CREATE INDEX CONCURRENTLY "s3_t_a_pos" ON "s3_t" ("a") WHERE a > 0;
[op 3] tx
  CREATE INDEX "s3_t_ab_idx" ON "s3_t" ("a", "b");
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_t` ADD INDEX `s3_t_a_idx` (`a`), ALGORITHM=INPLACE, LOCK=NONE;
[op 1] autocommit
  ALTER TABLE `s3_t` ADD UNIQUE INDEX `s3_t_b_uq` (`b`), ALGORITHM=INPLACE, LOCK=NONE;
[op 3] autocommit
  ALTER TABLE `s3_t` ADD INDEX `s3_t_ab_idx` (`a`, `b`), ALGORITHM=INPLACE, LOCK=NONE;
-- sqlite
[op 0] tx
  CREATE INDEX "s3_t_a_idx" ON "s3_t" ("a");
[op 1] tx
  CREATE UNIQUE INDEX "s3_t_b_uq" ON "s3_t" ("b");
[op 2] tx
  CREATE INDEX "s3_t_a_pos" ON "s3_t" ("a") WHERE a > 0;
[op 3] tx
  CREATE INDEX "s3_t_ab_idx" ON "s3_t" ("a", "b");
