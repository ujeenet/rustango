-- postgres
[op 0] tx
  ALTER TABLE "s3_t" RENAME COLUMN "name" TO "full_name";
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_t` RENAME COLUMN `name` TO `full_name`;
-- sqlite
[op 0] tx
  ALTER TABLE "s3_t" RENAME COLUMN "name" TO "full_name";
