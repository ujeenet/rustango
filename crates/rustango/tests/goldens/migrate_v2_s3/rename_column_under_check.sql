-- postgres
[op 0] tx
  ALTER TABLE "s3_t" DROP CONSTRAINT IF EXISTS "s3_t_a_pos";
[op 1] tx
  ALTER TABLE "s3_t" RENAME COLUMN "a" TO "b";
[op 2] tx
  ALTER TABLE "s3_t" ADD CONSTRAINT "s3_t_b_pos" CHECK (b > 0) NOT VALID;
[op 2] tx
  ALTER TABLE "s3_t" VALIDATE CONSTRAINT "s3_t_b_pos";
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_t` DROP /*!80000 CHECK */ /*M! CONSTRAINT */ `s3_t_a_pos`;
[op 1] autocommit
  ALTER TABLE `s3_t` RENAME COLUMN `a` TO `b`;
[op 2] autocommit
  ALTER TABLE `s3_t` ADD CONSTRAINT `s3_t_b_pos` CHECK (b > 0);
-- sqlite
[op 0] rebuild
  CREATE TABLE "_rustango_rebuild_s3_t" ("id" INTEGER NOT NULL, "a" INTEGER, PRIMARY KEY ("id"));
  INSERT INTO "_rustango_rebuild_s3_t" ("id", "a") SELECT "id", "a" FROM "s3_t";
  DROP TABLE "s3_t";
  ALTER TABLE "_rustango_rebuild_s3_t" RENAME TO "s3_t";
[op 1] tx
  ALTER TABLE "s3_t" RENAME COLUMN "a" TO "b";
[op 2] rebuild
  CREATE TABLE "_rustango_rebuild_s3_t" ("id" INTEGER NOT NULL, "b" INTEGER, PRIMARY KEY ("id"), CONSTRAINT "s3_t_b_pos" CHECK (b > 0));
  INSERT INTO "_rustango_rebuild_s3_t" ("id", "b") SELECT "id", "b" FROM "s3_t";
  DROP TABLE "s3_t";
  ALTER TABLE "_rustango_rebuild_s3_t" RENAME TO "s3_t";
