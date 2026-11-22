-- postgres
[op 0] tx
  ALTER TABLE "s3_parent" RENAME TO "s3_renamed";
  ALTER INDEX "s3_parent_pkey" RENAME TO "s3_renamed_pkey";
  ALTER SEQUENCE "s3_parent_id_seq" RENAME TO "s3_renamed_id_seq";
[op 1] tx
  CREATE TABLE "s3_parent" ("id" BIGSERIAL NOT NULL, "name" VARCHAR(50), PRIMARY KEY ("id"));
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_parent` RENAME TO `s3_renamed`;
[op 1] autocommit
  CREATE TABLE `s3_parent` (`id` BIGINT AUTO_INCREMENT NOT NULL, `name` VARCHAR(50), PRIMARY KEY (`id`));
-- sqlite
[op 0] tx
  ALTER TABLE "s3_parent" RENAME TO "s3_renamed";
[op 1] tx
  CREATE TABLE "s3_parent" ("id" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, "name" TEXT);
