-- postgres
[op 0] tx
  ALTER TABLE "s3_parent" ADD COLUMN "nick" VARCHAR(20);
  COMMENT ON COLUMN "s3_parent"."nick" IS 'short name';
[op 1] tx
  ALTER TABLE "s3_parent" ADD COLUMN "flag" BOOLEAN DEFAULT TRUE NOT NULL;
[op 2] tx
  ALTER TABLE "s3_parent" ADD COLUMN "score" INTEGER DEFAULT 0 NOT NULL;
  ALTER TABLE "s3_parent" ALTER COLUMN "score" DROP DEFAULT;
[op 3] tx
  ALTER TABLE "s3_parent" ADD COLUMN "created" TIMESTAMPTZ DEFAULT now();
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_parent` ADD COLUMN `nick` VARCHAR(20) COMMENT 'short name';
[op 1] autocommit
  ALTER TABLE `s3_parent` ADD COLUMN `flag` TINYINT(1) DEFAULT TRUE NOT NULL;
[op 2] autocommit
  ALTER TABLE `s3_parent` ADD COLUMN `score` INT;
[op 2] tx
  UPDATE `s3_parent` SET `score` = 0;
[op 2] autocommit
  ALTER TABLE `s3_parent` MODIFY COLUMN `score` INT NOT NULL;
[op 3] autocommit
  ALTER TABLE `s3_parent` ADD COLUMN `created` DATETIME(6) DEFAULT (UTC_TIMESTAMP(6));
-- sqlite
[op 0] tx
  ALTER TABLE "s3_parent" ADD COLUMN "nick" TEXT;
[op 1] tx
  ALTER TABLE "s3_parent" ADD COLUMN "flag" INTEGER DEFAULT TRUE NOT NULL;
[op 2] rebuild
  CREATE TABLE "_rustango_rebuild_s3_parent" ("id" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, "name" TEXT, "nick" TEXT, "flag" INTEGER DEFAULT TRUE NOT NULL, "score" INTEGER NOT NULL);
  INSERT INTO "_rustango_rebuild_s3_parent" ("id", "name", "nick", "flag", "score") SELECT "id", "name", "nick", "flag", 0 FROM "s3_parent";
  DELETE FROM sqlite_sequence WHERE name = '_rustango_rebuild_s3_parent';
  INSERT INTO sqlite_sequence (name, seq) SELECT '_rustango_rebuild_s3_parent', seq FROM sqlite_sequence WHERE name = 's3_parent';
  DROP TABLE "s3_parent";
  ALTER TABLE "_rustango_rebuild_s3_parent" RENAME TO "s3_parent";
[op 3] rebuild
  CREATE TABLE "_rustango_rebuild_s3_parent" ("id" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, "name" TEXT, "nick" TEXT, "flag" INTEGER DEFAULT TRUE NOT NULL, "score" INTEGER NOT NULL, "created" TEXT DEFAULT (strftime('%Y-%m-%dT%H:%M:%f000+00:00','now')));
  INSERT INTO "_rustango_rebuild_s3_parent" ("id", "name", "nick", "flag", "score", "created") SELECT "id", "name", "nick", "flag", "score", (strftime('%Y-%m-%dT%H:%M:%f000+00:00','now')) FROM "s3_parent";
  DELETE FROM sqlite_sequence WHERE name = '_rustango_rebuild_s3_parent';
  INSERT INTO sqlite_sequence (name, seq) SELECT '_rustango_rebuild_s3_parent', seq FROM sqlite_sequence WHERE name = 's3_parent';
  DROP TABLE "s3_parent";
  ALTER TABLE "_rustango_rebuild_s3_parent" RENAME TO "s3_parent";
