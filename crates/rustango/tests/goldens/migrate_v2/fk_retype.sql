-- postgres
[op 0] tx
  ALTER TABLE "s3_child" DROP CONSTRAINT "s3_child_pid_fk";
[op 1] tx
  ALTER TABLE "s3_child" ALTER COLUMN "pid" DROP DEFAULT;
  ALTER TABLE "s3_child" ALTER COLUMN "pid" TYPE BIGINT USING "pid"::BIGINT;
[op 2] tx
  ALTER TABLE "s3_t" ALTER COLUMN "id" DROP DEFAULT;
  ALTER TABLE "s3_t" ALTER COLUMN "id" TYPE BIGINT USING "id"::BIGINT;
[op 3] tx
  ALTER TABLE "s3_child" ADD CONSTRAINT "s3_child_pid_fk" FOREIGN KEY ("pid") REFERENCES "s3_t" ("id") NOT VALID;
[op 3] tx
  ALTER TABLE "s3_child" VALIDATE CONSTRAINT "s3_child_pid_fk";
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_child` DROP FOREIGN KEY `s3_child_pid_fk`;
[op 1] autocommit
  ALTER TABLE `s3_child` MODIFY COLUMN `pid` BIGINT;
[op 2] autocommit
  ALTER TABLE `s3_t` MODIFY COLUMN `id` BIGINT NOT NULL;
[op 3] autocommit
  ALTER TABLE `s3_child` ADD CONSTRAINT `s3_child_pid_fk` FOREIGN KEY (`pid`) REFERENCES `s3_t` (`id`);
-- sqlite
[op 0] rebuild
  CREATE TABLE "_rustango_rebuild_s3_child" ("id" INTEGER NOT NULL, "pid" INTEGER, PRIMARY KEY ("id"));
  INSERT INTO "_rustango_rebuild_s3_child" ("id", "pid") SELECT "id", "pid" FROM "s3_child";
  DROP TABLE "s3_child";
  ALTER TABLE "_rustango_rebuild_s3_child" RENAME TO "s3_child";
  CREATE INDEX "s3_child_pid_idx" ON "s3_child" ("pid");
[op 1] rebuild
  CREATE TABLE "_rustango_rebuild_s3_child" ("id" INTEGER NOT NULL, "pid" INTEGER, PRIMARY KEY ("id"));
  INSERT INTO "_rustango_rebuild_s3_child" ("id", "pid") SELECT "id", "pid" FROM "s3_child";
  DROP TABLE "s3_child";
  ALTER TABLE "_rustango_rebuild_s3_child" RENAME TO "s3_child";
  CREATE INDEX "s3_child_pid_idx" ON "s3_child" ("pid");
[op 2] rebuild
  CREATE TABLE "_rustango_rebuild_s3_t" ("id" INTEGER NOT NULL, PRIMARY KEY ("id"));
  INSERT INTO "_rustango_rebuild_s3_t" ("id") SELECT "id" FROM "s3_t";
  DROP TABLE "s3_t";
  ALTER TABLE "_rustango_rebuild_s3_t" RENAME TO "s3_t";
[op 3] rebuild
  CREATE TABLE "_rustango_rebuild_s3_child" ("id" INTEGER NOT NULL, "pid" INTEGER, PRIMARY KEY ("id"), CONSTRAINT "s3_child_pid_fk" FOREIGN KEY ("pid") REFERENCES "s3_t" ("id"));
  INSERT INTO "_rustango_rebuild_s3_child" ("id", "pid") SELECT "id", "pid" FROM "s3_child";
  DROP TABLE "s3_child";
  ALTER TABLE "_rustango_rebuild_s3_child" RENAME TO "s3_child";
  CREATE INDEX "s3_child_pid_idx" ON "s3_child" ("pid");
