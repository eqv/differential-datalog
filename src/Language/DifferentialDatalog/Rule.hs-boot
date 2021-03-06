module Language.DifferentialDatalog.Rule where

import qualified Data.Map as M

import Language.DifferentialDatalog.Syntax
import Language.DifferentialDatalog.Var

ruleRHSVars :: DatalogProgram -> Rule -> Int -> [Var]
ruleVars :: DatalogProgram -> Rule -> [Var]
ruleRHSTermVars :: DatalogProgram -> Rule -> Int -> [Var]
ruleLHSVars :: DatalogProgram -> Rule -> [Var]
ruleTypeMapM :: (Monad m) => (Type -> m Type) -> Rule -> m Rule
ruleHasJoins :: Rule -> Bool
ruleIsDistinctByConstruction :: DatalogProgram -> Rule -> Int -> Bool
ruleHeadIsRecursive :: DatalogProgram -> Rule -> Int -> Bool
ruleIsRecursive :: DatalogProgram -> Rule -> Bool
ruleAggregateTypeParams :: DatalogProgram -> Rule -> Int -> M.Map String Type
ruleAggregateKeyType :: DatalogProgram -> Rule -> Int -> Type
ruleAggregateValType :: DatalogProgram -> Rule -> Int -> Type
