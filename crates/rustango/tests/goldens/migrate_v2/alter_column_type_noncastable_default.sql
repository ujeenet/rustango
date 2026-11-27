-- postgres
[op 0] tx
  ALTER TABLE "s3_t" ALTER COLUMN "code" TYPE TEXT;
  ALTER TABLE "s3_t" ALTER COLUMN "code" DROP DEFAULT;
  ALTER TABLE "s3_t" ALTER COLUMN "code" TYPE BIGINT USING "code"::BIGINT;
  ALTER TABLE "s3_t" ALTER COLUMN "code" SET DEFAULT 0;
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_t` MODIFY COLUMN `code` BIGINT DEFAULT 0;
-- sqlite
[op 0] rebuild
  CREATE TABLE "_rustango_rebuild_s3_t" ("id" INTEGER NOT NULL, "code" INTEGER DEFAULT 0, PRIMARY KEY ("id"));
  INSERT INTO "_rustango_rebuild_s3_t" ("id", "code") SELECT "id", "code" FROM "s3_t";
  DROP TABLE "s3_t";
  ALTER TABLE "_rustango_rebuild_s3_t" RENAME TO "s3_t";
