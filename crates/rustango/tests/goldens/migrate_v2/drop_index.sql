-- postgres
[op 0] autocommit
  DROP INDEX CONCURRENTLY "s3_t_a_idx";
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_t` DROP INDEX `s3_t_a_idx`;
-- sqlite
[op 0] tx
  DROP INDEX "s3_t_a_idx";
