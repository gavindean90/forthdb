import unittest
from forthdb_transaction_ast import (
    TransactionAST,
    AllocateOp,
    ExpectWorldOp,
    ExpectObjectOp,
    DefineOp,
    ForgetOp,
    RejectOp,
    EntityId,
    LiteralRef,
    SymbolRef,
    WorldId
)

class TestTransactionAST(unittest.TestCase):
    def test_empty_transaction_golden(self):
        ast = TransactionAST(42, [])
        out = ast.lower_to_sisa()
        # header: version(4) + namespace(8) + slots_len(4) + preds_len(4) + lits_len(4) + local_count(4) + no dict + insts_len(4)
        self.assertEqual(len(out), 4 + 8 + 4 + 4 + 4 + 4 + 4)
        
    def test_validation_rejects_duplicate_symbol(self):
        ast = TransactionAST(42, [
            AllocateOp("foo"),
            AllocateOp("foo")
        ])
        with self.assertRaisesRegex(ValueError, "duplicate allocation for symbol 'foo'"):
            ast.lower_to_sisa()

    def test_validation_rejects_undefined_symbol(self):
        ast = TransactionAST(42, [
            DefineOp("status", SymbolRef("undefined"), "is", LiteralRef("available"))
        ])
        with self.assertRaisesRegex(ValueError, "use of undefined symbol 'undefined'"):
            ast.lower_to_sisa()

if __name__ == '__main__':
    unittest.main()
