-- postgres
[op 0] tx
  ALTER INDEX "s3_t_a_idx" RENAME TO "s3_t_a_idx2";
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_t` RENAME INDEX `s3_t_a_idx` TO `s3_t_a_idx2`;
-- sqlite
[op 0] tx
  DROP INDEX "s3_t_a_idx";
  CREATE INDEX "s3_t_a_idx2" ON "s3_t" ("a");
