from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from typing import Mapping, Protocol

from deployment_demo import DeploymentConstraint, DeploymentEntities
from forthdb_kernel import EntityId, Fact, ForthDB, Literal, Pattern, Predicate, SlotId, Symbol, Variable
from forthdb_world import CommittedWorldDB, ConstraintViolation, TransactionConflict, WorldTransaction


class Reader(Protocol):
    def resolve(self, slot: SlotId) -> Fact | None: ...
    def query(self, patterns, **kwargs): ...
    def definitions(self, slot: SlotId) -> tuple[Fact, ...]: ...


class WorldDeploymentHarness:
    NS = "deployment"

    def __init__(self, path: Path) -> None:
        self.db = CommittedWorldDB.open(path)
        if self.db.version == 0:
            self.entities = self._seed_entities()
            self.targets: Mapping[EntityId, EntityId] = {
                self.entities.api: self.entities.production_api,
                self.entities.worker: self.entities.production_worker,
                self.entities.schema: self.entities.production_schema,
            }
            self._seed_catalog_and_initial_world()
        else:
            self.entities = self._resolve_entities()
            self.targets = {
                self.entities.api: self.entities.production_api,
                self.entities.worker: self.entities.production_worker,
                self.entities.schema: self.entities.production_schema,
            }

    @staticmethod
    def relation_slot(owner: EntityId, relation: str, suffix: str = "current") -> SlotId:
        return SlotId(f"{owner.value}/{relation}/{suffix}")

    def desired_slot(self, target: EntityId) -> SlotId:
        return self.relation_slot(target, "desired_version")

    def observed_slot(self, target: EntityId) -> SlotId:
        return self.relation_slot(target, "observed_version")

    def current_release_slot(self, environment: EntityId) -> SlotId:
        return self.relation_slot(environment, "current_release")

    def _seed_entities(self) -> DeploymentEntities:
        values = [
            ("API", "API"),
            ("Worker", "Worker"),
            ("Schema", "Schema"),
            ("API_v2", "API v2"),
            ("API_v3", "API v3"),
            ("Worker_v6", "Worker v6"),
            ("Worker_v7", "Worker v7"),
            ("Schema_v4", "Schema v4"),
            ("Schema_v5", "Schema v5"),
            ("Production", "Production"),
            ("Staging", "Staging"),
            ("Production_API", "Production / API"),
            ("Production_Worker", "Production / Worker"),
            ("Production_Schema", "Production / Schema"),
            ("Release_11", "Release 11"),
            ("Release_12", "Release 12"),
            ("Release_Bad", "Broken Release"),
            ("Rollback_13", "Rollback 13"),
            ("Gavin", "Gavin"),
        ]
        entities: dict[str, EntityId] = {}
        with self.db.transaction() as tx:
            for symbol, display in values:
                entity = tx.entity()
                entities[symbol] = entity
                self.db.define_display_name(tx, entity, display)
                self.db.bind_symbol(tx, self.NS, Symbol(symbol), entity)
        return DeploymentEntities(
            api=entities["API"],
            worker=entities["Worker"],
            schema=entities["Schema"],
            api_v2=entities["API_v2"],
            api_v3=entities["API_v3"],
            worker_v6=entities["Worker_v6"],
            worker_v7=entities["Worker_v7"],
            schema_v4=entities["Schema_v4"],
            schema_v5=entities["Schema_v5"],
            production=entities["Production"],
            staging=entities["Staging"],
            production_api=entities["Production_API"],
            production_worker=entities["Production_Worker"],
            production_schema=entities["Production_Schema"],
            release_11=entities["Release_11"],
            release_12=entities["Release_12"],
            release_bad=entities["Release_Bad"],
            rollback_13=entities["Rollback_13"],
            gavin=entities["Gavin"],
        )

    def _resolve_entities(self) -> DeploymentEntities:
        def resolve(name: str) -> EntityId:
            pattern = self.db.compile_pattern(self.NS, Symbol(name), "display_name", Variable("name"))
            if not isinstance(pattern.subject, EntityId):
                raise AssertionError(name)
            return pattern.subject

        return DeploymentEntities(
            api=resolve("API"),
            worker=resolve("Worker"),
            schema=resolve("Schema"),
            api_v2=resolve("API_v2"),
            api_v3=resolve("API_v3"),
            worker_v6=resolve("Worker_v6"),
            worker_v7=resolve("Worker_v7"),
            schema_v4=resolve("Schema_v4"),
            schema_v5=resolve("Schema_v5"),
            production=resolve("Production"),
            staging=resolve("Staging"),
            production_api=resolve("Production_API"),
            production_worker=resolve("Production_Worker"),
            production_schema=resolve("Production_Schema"),
            release_11=resolve("Release_11"),
            release_12=resolve("Release_12"),
            release_bad=resolve("Release_Bad"),
            rollback_13=resolve("Rollback_13"),
            gavin=resolve("Gavin"),
        )

    @staticmethod
    def _define(tx: WorldTransaction, slot: SlotId, subject: EntityId, predicate: str, object_: EntityId) -> None:
        tx.define(slot, Fact(subject, Predicate(predicate), object_))

    def _seed_catalog_and_initial_world(self) -> None:
        e = self.entities
        with self.db.transaction() as tx:
            versions = (
                (e.api_v2, e.api, "api-v2"),
                (e.api_v3, e.api, "api-v3"),
                (e.worker_v6, e.worker, "worker-v6"),
                (e.worker_v7, e.worker, "worker-v7"),
                (e.schema_v4, e.schema, "schema-v4"),
                (e.schema_v5, e.schema, "schema-v5"),
            )
            for version, service, suffix in versions:
                self._define(tx, self.relation_slot(version, "version_of", suffix), version, "version_of", service)

            self._define(tx, self.relation_slot(e.api_v3, "requires", "worker-v7"), e.api_v3, "requires", e.worker_v7)
            self._define(tx, self.relation_slot(e.worker_v7, "requires", "schema-v5"), e.worker_v7, "requires", e.schema_v5)

            for service, target in self.targets.items():
                self._define(tx, self.relation_slot(target, "environment"), target, "in_environment", e.production)
                self._define(tx, self.relation_slot(target, "service"), target, "targets_service", service)

            self.stage_release(
                tx,
                e.release_11,
                {e.api: e.api_v2, e.worker: e.worker_v6, e.schema: e.schema_v4},
            )
            for service, version in (
                (e.api, e.api_v2),
                (e.worker, e.worker_v6),
                (e.schema, e.schema_v4),
            ):
                self.stage_observed(tx, service, version)
            tx.require(lambda candidate: self.validate_release(candidate, e.release_11))

    def stage_release(
        self,
        tx: WorldTransaction,
        deployment: EntityId,
        versions_by_service: Mapping[EntityId, EntityId],
    ) -> None:
        e = self.entities
        self._define(tx, self.relation_slot(deployment, "environment"), deployment, "target_environment", e.production)
        self._define(tx, self.relation_slot(deployment, "approval"), deployment, "approved_by", e.gavin)
        for service, version in versions_by_service.items():
            self._define(
                tx,
                self.relation_slot(deployment, "deploys", str(service.value)),
                deployment,
                "deploys",
                version,
            )
            target = self.targets[service]
            tx.define(self.desired_slot(target), Fact(target, Predicate("desired_version"), version))
        tx.define(
            self.current_release_slot(e.production),
            Fact(e.production, Predicate("current_release"), deployment),
        )

    def stage_observed(self, tx: WorldTransaction, service: EntityId, version: EntityId) -> None:
        target = self.targets[service]
        tx.define(self.observed_slot(target), Fact(target, Predicate("observed_version"), version))

    @staticmethod
    def _display(reader: Reader, entity: EntityId) -> str:
        fact = reader.resolve(ForthDB.display_slot(entity))
        if fact is None or not isinstance(fact.object, Literal):
            return str(entity)
        return fact.object.value

    def release_versions(self, reader: Reader, deployment: EntityId) -> dict[EntityId, EntityId]:
        result = reader.query(
            [
                Pattern(deployment, Predicate("deploys"), Variable("version")),
                Pattern(Variable("version"), Predicate("version_of"), Variable("service")),
            ]
        )
        versions: dict[EntityId, EntityId] = {}
        for row in result.rows:
            service = row.binding["service"]
            version = row.binding["version"]
            if not isinstance(service, EntityId) or not isinstance(version, EntityId):
                raise DeploymentConstraint("Release relationships must resolve to entities")
            if service in versions:
                raise DeploymentConstraint("A release may select only one version per service")
            versions[service] = version
        return versions

    def validate_release(self, candidate: ForthDB, deployment: EntityId) -> None:
        e = self.entities
        approval = candidate.query([Pattern(deployment, Predicate("approved_by"), Variable("approver"))])
        if not approval.rows:
            raise DeploymentConstraint("Deployment requires approval")

        target_environment = candidate.query(
            [Pattern(deployment, Predicate("target_environment"), Variable("environment"))]
        ).bindings()
        if target_environment != [{"environment": e.production}]:
            raise DeploymentConstraint("Deployment must target Production exactly once")

        selected = self.release_versions(candidate, deployment)
        if set(selected) != set(self.targets):
            raise DeploymentConstraint("Deployment must select exactly one version for every service")

        selected_versions = set(selected.values())
        for version in selected_versions:
            for binding in candidate.query(
                [Pattern(version, Predicate("requires"), Variable("required"))]
            ).bindings():
                if binding["required"] not in selected_versions:
                    raise DeploymentConstraint("Release dependency is not included")

        for service, target in self.targets.items():
            desired = candidate.resolve(self.desired_slot(target))
            if desired is None or desired.object != selected[service]:
                raise DeploymentConstraint("Desired state must match release manifest")

        current = candidate.resolve(self.current_release_slot(e.production))
        if current is None or current.object != deployment:
            raise DeploymentConstraint("Production must point at candidate release")

    def state(self, reader: Reader, relation: str) -> dict[str, str]:
        result: dict[str, str] = {}
        for service, target in self.targets.items():
            slot = self.desired_slot(target) if relation == "desired" else self.observed_slot(target)
            fact = reader.resolve(slot)
            if fact is None or not isinstance(fact.object, EntityId):
                raise DeploymentConstraint(f"Target has no {relation} version")
            result[self._display(reader, service)] = self._display(reader, fact.object)
        return dict(sorted(result.items()))

    def drift(self, reader: Reader) -> list[dict[str, str]]:
        desired = self.state(reader, "desired")
        observed = self.state(reader, "observed")
        return [
            {"service": service, "desired": desired[service], "observed": observed[service]}
            for service in sorted(desired)
            if desired[service] != observed[service]
        ]

    def current_release(self, reader: Reader) -> str:
        fact = reader.resolve(self.current_release_slot(self.entities.production))
        if fact is None or not isinstance(fact.object, EntityId):
            raise DeploymentConstraint("Production has no current release")
        return self._display(reader, fact.object)

    def desired_history(self, reader: Reader, service: EntityId) -> list[str]:
        return [
            self._display(reader, fact.object)
            for fact in reader.definitions(self.desired_slot(self.targets[service]))
            if isinstance(fact.object, EntityId)
        ]


def run_deployment(path: Path) -> dict:
    app = WorldDeploymentHarness(path)
    db = app.db
    e = app.entities

    initial_desired = app.state(db, "desired")
    initial_observed = app.state(db, "observed")

    version_before_rejection = db.version
    size_before_rejection = path.stat().st_size
    bad_tx = db.transaction()
    app.stage_release(
        bad_tx,
        e.release_bad,
        {e.api: e.api_v3, e.worker: e.worker_v6, e.schema: e.schema_v4},
    )
    bad_tx.require(lambda candidate: app.validate_release(candidate, e.release_bad))
    incompatible_rejected = False
    try:
        bad_tx.commit()
    except ConstraintViolation:
        incompatible_rejected = True
    rejection_preserved_world = (
        db.version == version_before_rejection and path.stat().st_size == size_before_rejection
    )

    before_release = db.snapshot()
    with db.transaction() as tx:
        app.stage_release(
            tx,
            e.release_12,
            {e.api: e.api_v3, e.worker: e.worker_v7, e.schema: e.schema_v5},
        )
        tx.require(lambda candidate: app.validate_release(candidate, e.release_12))

    release_12_desired = app.state(db, "desired")
    old_snapshot_desired = app.state(before_release, "desired")
    drift_after_release = app.drift(db)

    with db.transaction() as tx:
        app.stage_observed(tx, e.worker, e.worker_v7)
    drift_after_worker = app.drift(db)
    with db.transaction() as tx:
        app.stage_observed(tx, e.schema, e.schema_v5)
    drift_after_schema = app.drift(db)
    with db.transaction() as tx:
        app.stage_observed(tx, e.api, e.api_v3)
    drift_after_convergence = app.drift(db)

    with db.transaction() as tx:
        app.stage_release(
            tx,
            e.rollback_13,
            {e.api: e.api_v2, e.worker: e.worker_v6, e.schema: e.schema_v4},
        )
        tx.require(lambda candidate: app.validate_release(candidate, e.rollback_13))

    rollback_desired = app.state(db, "desired")
    rollback_drift = app.drift(db)
    pre_restart_version = db.version
    pre_restart_digest = db.digest

    recovered_app = WorldDeploymentHarness(path)
    recovered = recovered_app.db

    return {
        "initial_desired": initial_desired,
        "initial_observed": initial_observed,
        "incompatible_rejected": incompatible_rejected,
        "rejection_preserved_world": rejection_preserved_world,
        "old_snapshot_desired": old_snapshot_desired,
        "release_12_desired": release_12_desired,
        "drift_after_release": drift_after_release,
        "drift_after_worker": drift_after_worker,
        "drift_after_schema": drift_after_schema,
        "drift_after_convergence": drift_after_convergence,
        "rollback_desired": rollback_desired,
        "rollback_drift": rollback_drift,
        "current_release": app.current_release(db),
        "api_desired_history": app.desired_history(db, e.api),
        "world_version": db.version,
        "world_digest": db.digest,
        "active_slots": db.active_slots,
        "immutable_records": db.immutable_records,
        "recovery": {
            "same_version": recovered.version == pre_restart_version,
            "same_digest": recovered.digest == pre_restart_digest,
            "desired": recovered_app.state(recovered, "desired"),
            "drift": recovered_app.drift(recovered),
            "current_release": recovered_app.current_release(recovered),
        },
    }


class WorldDeploymentTests(unittest.TestCase):
    def test_deployment_application_and_recovery(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            report = run_deployment(Path(directory) / "deployment.fdb")
            self.assertTrue(report["incompatible_rejected"])
            self.assertTrue(report["rejection_preserved_world"])
            self.assertEqual(report["old_snapshot_desired"], report["initial_desired"])
            self.assertEqual(len(report["drift_after_release"]), 3)
            self.assertEqual(len(report["drift_after_worker"]), 2)
            self.assertEqual(len(report["drift_after_schema"]), 1)
            self.assertEqual(report["drift_after_convergence"], [])
            self.assertEqual(report["current_release"], "Rollback 13")
            self.assertEqual(report["api_desired_history"], ["API v2", "API v3", "API v2"])
            self.assertTrue(report["recovery"]["same_version"])
            self.assertTrue(report["recovery"]["same_digest"])
            self.assertEqual(report["recovery"]["desired"], report["rollback_desired"])
            self.assertEqual(report["recovery"]["drift"], report["rollback_drift"])

    def test_release_publication_is_one_world_transition(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            app = WorldDeploymentHarness(Path(directory) / "atomic-release.fdb")
            e = app.entities
            old = app.db.snapshot()
            with app.db.transaction() as tx:
                app.stage_release(
                    tx,
                    e.release_12,
                    {e.api: e.api_v3, e.worker: e.worker_v7, e.schema: e.schema_v5},
                )
                self.assertEqual(app.state(app.db, "desired"), app.state(old, "desired"))
                self.assertEqual(
                    app.state(tx.snapshot(), "desired"),
                    {"API": "API v3", "Schema": "Schema v5", "Worker": "Worker v7"},
                )
                tx.require(lambda candidate: app.validate_release(candidate, e.release_12))
            self.assertEqual(app.state(old, "desired"), {"API": "API v2", "Schema": "Schema v4", "Worker": "Worker v6"})
            self.assertEqual(app.state(app.db, "desired"), {"API": "API v3", "Schema": "Schema v5", "Worker": "Worker v7"})

    def test_stale_release_operator_aborts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            app = WorldDeploymentHarness(Path(directory) / "stale-release.fdb")
            e = app.entities
            first = app.db.transaction()
            second = app.db.transaction()
            app.stage_release(first, e.release_12, {e.api: e.api_v3, e.worker: e.worker_v7, e.schema: e.schema_v5})
            first.require(lambda candidate: app.validate_release(candidate, e.release_12))
            app.stage_release(second, e.rollback_13, {e.api: e.api_v2, e.worker: e.worker_v6, e.schema: e.schema_v4})
            second.require(lambda candidate: app.validate_release(candidate, e.rollback_13))
            first.commit()
            with self.assertRaises(TransactionConflict):
                second.commit()
            self.assertEqual(app.current_release(app.db), "Release 12")


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(WorldDeploymentTests)
    outcome = unittest.TextTestRunner(verbosity=2).run(suite)
    if not outcome.wasSuccessful():
        raise SystemExit(1)

    with tempfile.TemporaryDirectory() as directory:
        print("\n" + "=" * 78)
        print("COMMITTED-WORLD DEPLOYMENT CONTROL-PLANE HARNESS")
        print("=" * 78)
        print(json.dumps(run_deployment(Path(directory) / "deployment.fdb"), indent=2, sort_keys=True))
