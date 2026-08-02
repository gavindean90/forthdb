from __future__ import annotations

import json
import unittest
from dataclasses import dataclass
from typing import Mapping

from forthdb_kernel import (
    EntityId,
    Fact,
    ForthDB,
    Pattern,
    Predicate,
    SlotId,
    Symbol,
    Variable,
)


class DeploymentConstraint(ValueError):
    pass


@dataclass(frozen=True)
class DeploymentEntities:
    api: EntityId
    worker: EntityId
    schema: EntityId
    api_v2: EntityId
    api_v3: EntityId
    worker_v6: EntityId
    worker_v7: EntityId
    schema_v4: EntityId
    schema_v5: EntityId
    production: EntityId
    staging: EntityId
    production_api: EntityId
    production_worker: EntityId
    production_schema: EntityId
    release_11: EntityId
    release_12: EntityId
    release_bad: EntityId
    rollback_13: EntityId
    gavin: EntityId


class DeploymentHarness:
    NS = "deployment"

    def __init__(self) -> None:
        self.db = ForthDB()
        self.entities = self._seed_entities()
        self.targets: Mapping[EntityId, EntityId] = {
            self.entities.api: self.entities.production_api,
            self.entities.worker: self.entities.production_worker,
            self.entities.schema: self.entities.production_schema,
        }
        self._seed_catalog_and_initial_world()

    @staticmethod
    def relation_slot(owner: EntityId, relation: str, suffix: str = "current") -> SlotId:
        return SlotId(f"{owner.value}/{relation}/{suffix}")

    def named(self, symbol: str, display: str) -> EntityId:
        entity = self.db.entity()
        self.db.define_display_name(entity, display)
        self.db.bind_symbol(self.NS, Symbol(symbol), entity)
        return entity

    def _seed_entities(self) -> DeploymentEntities:
        return DeploymentEntities(
            api=self.named("API", "API"),
            worker=self.named("Worker", "Worker"),
            schema=self.named("Schema", "Schema"),
            api_v2=self.named("API_v2", "API v2"),
            api_v3=self.named("API_v3", "API v3"),
            worker_v6=self.named("Worker_v6", "Worker v6"),
            worker_v7=self.named("Worker_v7", "Worker v7"),
            schema_v4=self.named("Schema_v4", "Schema v4"),
            schema_v5=self.named("Schema_v5", "Schema v5"),
            production=self.named("Production", "Production"),
            staging=self.named("Staging", "Staging"),
            production_api=self.named("Production_API", "Production / API"),
            production_worker=self.named("Production_Worker", "Production / Worker"),
            production_schema=self.named("Production_Schema", "Production / Schema"),
            release_11=self.named("Release_11", "Release 11"),
            release_12=self.named("Release_12", "Release 12"),
            release_bad=self.named("Release_Bad", "Broken Release"),
            rollback_13=self.named("Rollback_13", "Rollback 13"),
            gavin=self.named("Gavin", "Gavin"),
        )

    def add(self, slot: SlotId, subject: EntityId, predicate: str, object_: EntityId) -> None:
        self.db.define(slot, Fact(subject, Predicate(predicate), object_))

    def desired_slot(self, target: EntityId) -> SlotId:
        return self.relation_slot(target, "desired_version")

    def observed_slot(self, target: EntityId) -> SlotId:
        return self.relation_slot(target, "observed_version")

    def current_release_slot(self, environment: EntityId) -> SlotId:
        return self.relation_slot(environment, "current_release")

    def _seed_catalog_and_initial_world(self) -> None:
        e = self.entities
        versions = (
            (e.api_v2, e.api, "api-v2"),
            (e.api_v3, e.api, "api-v3"),
            (e.worker_v6, e.worker, "worker-v6"),
            (e.worker_v7, e.worker, "worker-v7"),
            (e.schema_v4, e.schema, "schema-v4"),
            (e.schema_v5, e.schema, "schema-v5"),
        )
        for version, service, suffix in versions:
            self.add(self.relation_slot(version, "version_of", suffix), version, "version_of", service)

        self.add(self.relation_slot(e.api_v3, "requires", "worker-v7"), e.api_v3, "requires", e.worker_v7)
        self.add(self.relation_slot(e.worker_v7, "requires", "schema-v5"), e.worker_v7, "requires", e.schema_v5)

        for service, target in self.targets.items():
            self.add(self.relation_slot(target, "environment"), target, "in_environment", e.production)
            self.add(self.relation_slot(target, "service"), target, "targets_service", service)

        self.apply_release(
            e.release_11,
            {
                e.api: e.api_v2,
                e.worker: e.worker_v6,
                e.schema: e.schema_v4,
            },
        )
        for service, version in (
            (e.api, e.api_v2),
            (e.worker, e.worker_v6),
            (e.schema, e.schema_v4),
        ):
            self.report_observed(service, version)

    def apply_release(self, deployment: EntityId, versions_by_service: Mapping[EntityId, EntityId]) -> None:
        e = self.entities
        self.add(
            self.relation_slot(deployment, "environment"),
            deployment,
            "target_environment",
            e.production,
        )
        self.add(
            self.relation_slot(deployment, "approval"),
            deployment,
            "approved_by",
            e.gavin,
        )
        for service, version in versions_by_service.items():
            self.add(
                self.relation_slot(deployment, "deploys", str(service.value)),
                deployment,
                "deploys",
                version,
            )
            target = self.targets[service]
            self.db.define(
                self.desired_slot(target),
                Fact(target, Predicate("desired_version"), version),
            )
        self.db.define(
            self.current_release_slot(e.production),
            Fact(e.production, Predicate("current_release"), deployment),
        )

    def report_observed(self, service: EntityId, version: EntityId) -> None:
        target = self.targets[service]
        self.db.define(
            self.observed_slot(target),
            Fact(target, Predicate("observed_version"), version),
        )

    def release_versions(self, deployment: EntityId) -> dict[EntityId, EntityId]:
        result = self.db.query(
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

    def validate_release(self, deployment: EntityId) -> None:
        e = self.entities
        approval = self.db.query([Pattern(deployment, Predicate("approved_by"), Variable("approver"))])
        if not approval.rows:
            raise DeploymentConstraint("Deployment requires approval")

        target_environment = self.db.query(
            [Pattern(deployment, Predicate("target_environment"), Variable("environment"))]
        ).bindings()
        if target_environment != [{"environment": e.production}]:
            raise DeploymentConstraint("Deployment must target Production exactly once")

        selected = self.release_versions(deployment)
        expected_services = set(self.targets)
        if set(selected) != expected_services:
            raise DeploymentConstraint("Deployment must select exactly one version for every service")

        selected_versions = set(selected.values())
        for version in selected_versions:
            requirements = self.db.query(
                [Pattern(version, Predicate("requires"), Variable("required"))]
            ).bindings()
            for binding in requirements:
                required = binding["required"]
                if required not in selected_versions:
                    raise DeploymentConstraint(
                        f"{self.db.display_name(version)} requires {self.db.render_value(required)}"
                    )

        for service, target in self.targets.items():
            desired = self.db.resolve(self.desired_slot(target))
            if desired is None or desired.object != selected[service]:
                raise DeploymentConstraint("Desired state must match the release manifest")

        current_release = self.db.resolve(self.current_release_slot(e.production))
        if current_release is None or current_release.object != deployment:
            raise DeploymentConstraint("Production must point at the candidate release")

    def desired_state(self) -> dict[str, str]:
        state: dict[str, str] = {}
        for service, target in self.targets.items():
            fact = self.db.resolve(self.desired_slot(target))
            if fact is None or not isinstance(fact.object, EntityId):
                raise DeploymentConstraint("Target has no desired version")
            state[self.db.display_name(service)] = self.db.display_name(fact.object)
        return dict(sorted(state.items()))

    def observed_state(self) -> dict[str, str]:
        state: dict[str, str] = {}
        for service, target in self.targets.items():
            fact = self.db.resolve(self.observed_slot(target))
            if fact is None or not isinstance(fact.object, EntityId):
                raise DeploymentConstraint("Target has no observed version")
            state[self.db.display_name(service)] = self.db.display_name(fact.object)
        return dict(sorted(state.items()))

    def drift(self) -> list[dict[str, str]]:
        desired = self.desired_state()
        observed = self.observed_state()
        return [
            {"service": service, "desired": desired[service], "observed": observed[service]}
            for service in sorted(desired)
            if desired[service] != observed[service]
        ]

    def current_release(self) -> str:
        fact = self.db.resolve(self.current_release_slot(self.entities.production))
        if fact is None or not isinstance(fact.object, EntityId):
            raise DeploymentConstraint("Production has no current release")
        return self.db.display_name(fact.object)

    def desired_history(self, service: EntityId) -> list[str]:
        target = self.targets[service]
        return [
            self.db.display_name(fact.object)
            for fact in self.db.definitions(self.desired_slot(target))
            if isinstance(fact.object, EntityId)
        ]


def build_incompatible_candidate() -> DeploymentHarness:
    candidate = DeploymentHarness()
    e = candidate.entities
    candidate.apply_release(
        e.release_bad,
        {
            e.api: e.api_v3,
            e.worker: e.worker_v6,
            e.schema: e.schema_v4,
        },
    )
    return candidate


def run_deployment() -> dict:
    app = DeploymentHarness()
    e = app.entities

    initial_desired = app.desired_state()
    initial_observed = app.observed_state()

    incompatible_rejected = False
    incompatible = build_incompatible_candidate()
    try:
        incompatible.validate_release(incompatible.entities.release_bad)
    except DeploymentConstraint:
        incompatible_rejected = True

    app.apply_release(
        e.release_12,
        {
            e.api: e.api_v3,
            e.worker: e.worker_v7,
            e.schema: e.schema_v5,
        },
    )
    app.validate_release(e.release_12)
    release_12_desired = app.desired_state()
    drift_after_release = app.drift()

    app.report_observed(e.worker, e.worker_v7)
    drift_after_worker = app.drift()
    app.report_observed(e.schema, e.schema_v5)
    drift_after_schema = app.drift()
    app.report_observed(e.api, e.api_v3)
    drift_after_convergence = app.drift()

    app.apply_release(
        e.rollback_13,
        {
            e.api: e.api_v2,
            e.worker: e.worker_v6,
            e.schema: e.schema_v4,
        },
    )
    app.validate_release(e.rollback_13)
    rollback_desired = app.desired_state()
    rollback_drift = app.drift()

    app.db.validate()

    return {
        "initial_desired": initial_desired,
        "initial_observed": initial_observed,
        "incompatible_rejected": incompatible_rejected,
        "release_12_desired": release_12_desired,
        "drift_after_release": drift_after_release,
        "drift_after_worker": drift_after_worker,
        "drift_after_schema": drift_after_schema,
        "drift_after_convergence": drift_after_convergence,
        "rollback_desired": rollback_desired,
        "rollback_drift": rollback_drift,
        "current_release": app.current_release(),
        "api_desired_history": app.desired_history(e.api),
        "active_slots": len(app.db.store.head),
        "immutable_records": len(app.db.store.log),
    }


class DeploymentKernelTests(unittest.TestCase):
    def test_deployment_control_plane(self) -> None:
        report = run_deployment()
        self.assertTrue(report["incompatible_rejected"])
        self.assertEqual(
            report["release_12_desired"],
            {"API": "API v3", "Schema": "Schema v5", "Worker": "Worker v7"},
        )
        self.assertEqual(len(report["drift_after_release"]), 3)
        self.assertEqual(len(report["drift_after_worker"]), 2)
        self.assertEqual(len(report["drift_after_schema"]), 1)
        self.assertEqual(report["drift_after_convergence"], [])
        self.assertEqual(report["current_release"], "Rollback 13")
        self.assertEqual(report["api_desired_history"], ["API v2", "API v3", "API v2"])

    def test_incompatible_candidate_does_not_change_baseline(self) -> None:
        baseline = DeploymentHarness()
        before = baseline.desired_state()
        candidate = build_incompatible_candidate()
        with self.assertRaises(DeploymentConstraint):
            candidate.validate_release(candidate.entities.release_bad)
        self.assertEqual(baseline.desired_state(), before)
        self.assertEqual(baseline.current_release(), "Release 11")


if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(DeploymentKernelTests)
    outcome = unittest.TextTestRunner(verbosity=2).run(suite)
    if not outcome.wasSuccessful():
        raise SystemExit(1)

    print("\n" + "=" * 78)
    print("DEPLOYMENT CONTROL-PLANE HARNESS")
    print("=" * 78)
    print(json.dumps(run_deployment(), indent=2, sort_keys=True))
