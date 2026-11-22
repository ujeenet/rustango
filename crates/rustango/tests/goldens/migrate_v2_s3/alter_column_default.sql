-- postgres
[op 0] tx
  ALTER TABLE "s3_t" ALTER COLUMN "status" SET DEFAULT 'open';
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_t` MODIFY COLUMN `status` VARCHAR(10) DEFAULT 'open' NOT NULL;
-- sqlite
[op 0] rebuild
  CREATE TABLE "_rustango_rebuild_s3_t" ("id" INTEGER NOT NULL, "status" TEXT DEFAULT 'open' NOT NULL, PRIMARY KEY ("id"));
  INSERT INTO "_rustango_rebuild_s3_t" ("id", "status") SELECT "id", "status" FROM "s3_t";
  DROP TABLE "s3_t";
  ALTER TABLE "_rustango_rebuild_s3_t" RENAME TO "s3_t";
