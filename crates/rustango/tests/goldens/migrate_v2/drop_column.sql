-- postgres
[op 0] tx
  ALTER TABLE "s3_t" DROP COLUMN "legacy";
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_t` DROP COLUMN `legacy`;
-- sqlite
[op 0] tx
  ALTER TABLE "s3_t" DROP COLUMN "legacy";
