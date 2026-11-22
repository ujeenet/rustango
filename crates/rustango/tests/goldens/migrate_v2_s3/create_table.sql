-- postgres
[op 0] tx
  CREATE TABLE "s3_child" ("id" BIGINT NOT NULL, "parent_id" BIGINT NOT NULL, "total" NUMERIC DEFAULT 0 NOT NULL, "note" VARCHAR(20), PRIMARY KEY ("id"), CONSTRAINT "s3_child_total_ge0" CHECK (total >= 0), CONSTRAINT "s3_child_parent_fk" FOREIGN KEY ("parent_id") REFERENCES "s3_parent" ("id") ON DELETE CASCADE);
  COMMENT ON TABLE "s3_child" IS 'children';
  COMMENT ON COLUMN "s3_child"."note" IS 'a note';
  CREATE INDEX "s3_child_parent_idx" ON "s3_child" ("parent_id");
-- mysql
[op 0] autocommit
  CREATE TABLE `s3_child` (`id` BIGINT NOT NULL, `parent_id` BIGINT NOT NULL, `total` DECIMAL(65, 28) DEFAULT 0 NOT NULL, `note` VARCHAR(20) COMMENT 'a note', PRIMARY KEY (`id`), CONSTRAINT `s3_child_total_ge0` CHECK (total >= 0), CONSTRAINT `s3_child_parent_fk` FOREIGN KEY (`parent_id`) REFERENCES `s3_parent` (`id`) ON DELETE CASCADE, INDEX `s3_child_parent_idx` (`parent_id`)) COMMENT='children';
-- sqlite
[op 0] tx
  CREATE TABLE "s3_child" ("id" INTEGER NOT NULL, "parent_id" INTEGER NOT NULL, "total" NUMERIC DEFAULT 0 NOT NULL, "note" TEXT, PRIMARY KEY ("id"), CONSTRAINT "s3_child_total_ge0" CHECK (total >= 0), CONSTRAINT "s3_child_parent_fk" FOREIGN KEY ("parent_id") REFERENCES "s3_parent" ("id") ON DELETE CASCADE);
  CREATE INDEX "s3_child_parent_idx" ON "s3_child" ("parent_id");
