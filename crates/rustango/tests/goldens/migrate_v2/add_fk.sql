-- postgres
[op 0] tx
  ALTER TABLE "s3_child" ADD CONSTRAINT "s3_child_parent_fk" FOREIGN KEY ("parent_id") REFERENCES "s3_parent" ("id") ON DELETE CASCADE NOT VALID;
[op 0] tx
  ALTER TABLE "s3_child" VALIDATE CONSTRAINT "s3_child_parent_fk";
-- mysql
[op 0] autocommit
  ALTER TABLE `s3_child` ADD CONSTRAINT `s3_child_parent_fk` FOREIGN KEY (`parent_id`) REFERENCES `s3_parent` (`id`) ON DELETE CASCADE;
-- sqlite
[op 0] rebuild
  CREATE TABLE "_rustango_rebuild_s3_child" ("id" INTEGER NOT NULL, "parent_id" INTEGER NOT NULL, PRIMARY KEY ("id"), CONSTRAINT "s3_child_parent_fk" FOREIGN KEY ("parent_id") REFERENCES "s3_parent" ("id") ON DELETE CASCADE);
  INSERT INTO "_rustango_rebuild_s3_child" ("id", "parent_id") SELECT "id", "parent_id" FROM "s3_child";
  DROP TABLE "s3_child";
  ALTER TABLE "_rustango_rebuild_s3_child" RENAME TO "s3_child";
  CREATE INDEX "s3_child_parent_idx" ON "s3_child" ("parent_id");
