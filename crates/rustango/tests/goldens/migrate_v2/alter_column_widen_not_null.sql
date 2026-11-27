-- postgres
[op 0] tx
  ALTER TABLE "s3_parent" ALTER COLUMN "name" TYPE VARCHAR(100);
[op 0] tx
  ALTER TABLE "s3_parent" ADD CONSTRAINT "s3_parent_name_v2nn" CHECK ("name" IS NOT NULL) NOT VALID;
[op 0] autocommit
  ALTER TABLE "s3_parent" VALIDATE CONSTRAINT "s3_parent_name_v2nn";
[op 0] tx
  ALTER TABLE "s3_parent" ALTER COLUMN "name" SET NOT NULL;
  ALTER TABLE "s3_parent" DROP CONSTRAINT "s3_parent_name_v2nn";
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_parent` MODIFY COLUMN `name` VARCHAR(100) NOT NULL;
-- sqlite
[op 0] rebuild
  CREATE TABLE "_rustango_rebuild_s3_parent" ("id" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, "name" TEXT NOT NULL);
  INSERT INTO "_rustango_rebuild_s3_parent" ("id", "name") SELECT "id", "name" FROM "s3_parent";
  DELETE FROM sqlite_sequence WHERE name = '_rustango_rebuild_s3_parent';
  INSERT INTO sqlite_sequence (name, seq) SELECT '_rustango_rebuild_s3_parent', seq FROM sqlite_sequence WHERE name = 's3_parent';
  DROP TABLE "s3_parent";
  ALTER TABLE "_rustango_rebuild_s3_parent" RENAME TO "s3_parent";
